//! Repo microagents (OpenHands microagent pattern, arsenal B2).
//!
//! A microagent is a small markdown file that carries *repo-specific*
//! instructions ("this crate forbids unwraps outside tests", "run
//! `just check` before finishing"). OpenHands keeps a `microagents/`
//! directory whose entries are injected when the user's message matches
//! the agent's triggers; this is the same contract with the same
//! progressive-disclosure bargain as skills: the metadata is cheap, the
//! body only enters context when it is relevant.
//!
//! Layout (walked recursively, depth-bounded):
//!
//! ```text
//! <cwd>/.overseer/microagents/**/MICROAGENT.md
//! ```
//!
//! Frontmatter (the same `key: value` grammar skills use):
//!
//! ```markdown
//! ---
//! name: rust-style
//! triggers: unwrap, clippy, style
//! ---
//! Never introduce `unwrap()` outside tests…
//! ```
//!
//! `triggers` is comma-separated and repeatable; a trigger matches when it
//! appears as a case-insensitive substring of the user's prompt. The
//! literal trigger `always` matches every prompt (the always-on form).
//!
//! Injection is one harness-authored `Nudge` per matching microagent, so
//! the body is on the event log and replays identically on resume — the
//! same durability contract as every other Nudge.
//! `// DEFERRED(owner): nested-repo microagents (a submodule's own
//! `.overseer/microagents/`) — the walk is depth-bounded from the session
//! cwd; extend if monorepo operators ask.`

use std::path::{Path, PathBuf};

/// The filename that marks a microagent directory entry.
pub const MICROAGENT_FILE: &str = "MICROAGENT.md";

/// Max directory depth walked under `.overseer/microagents` — a bounded
/// scan keeps a pathological tree from costing a turn.
const MAX_DEPTH: usize = 4;

/// Trigger that matches every prompt (the always-on microagent).
pub const ALWAYS_TRIGGER: &str = "always";

/// One discovered microagent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Microagent {
    pub name: String,
    /// Directory that held the file — support files live beside it.
    pub path: PathBuf,
    pub triggers: Vec<String>,
    pub body: String,
}

/// `---`-delimited frontmatter → (name, triggers, body-less remainder).
/// Mirrors `skills::parse_frontmatter`: `key: value` lines only, no YAML
/// dependency for two fields. A file with no frontmatter is skipped (a
/// microagent without a name cannot be addressed or deduped).
fn parse(text: &str) -> Option<(String, Vec<String>)> {
    let t = text.strip_prefix("---")?;
    let fm = t.split("\n---").next()?;
    let mut name = None;
    let mut triggers: Vec<String> = Vec::new();
    for line in fm.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'');
        match k.trim() {
            "name" => name = Some(v.to_string()),
            "trigger" | "triggers" => {
                for part in v.split(',') {
                    let p = part.trim();
                    if !p.is_empty() {
                        triggers.push(p.to_string());
                    }
                }
            }
            _ => {}
        }
    }
    let name = name?;
    triggers.sort();
    triggers.dedup();
    Some((name, triggers))
}

/// The body after the frontmatter block.
fn body_of(text: &str) -> String {
    text.splitn(3, "---")
        .nth(2)
        .unwrap_or(text)
        .trim()
        .to_string()
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<Microagent>) {
    if depth > MAX_DEPTH {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            children.push(p);
            continue;
        }
        if e.file_name() != MICROAGENT_FILE {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        if let Some((name, triggers)) = parse(&text) {
            out.push(Microagent {
                name,
                path: p,
                triggers,
                body: body_of(&text),
            });
        }
    }
    // Deterministic order regardless of readdir order.
    children.sort();
    for c in children {
        walk(&c, depth + 1, out);
    }
}

/// Every microagent under the standard root, sorted by name. Name
/// collisions keep the first (shallowest, then alphabetical) — the same
/// first-hit rule skills use.
pub fn scan(cwd: &Path) -> Vec<Microagent> {
    let mut found = Vec::new();
    walk(&cwd.join(".overseer/microagents"), 0, &mut found);
    let mut seen = std::collections::HashSet::new();
    found.retain(|m| seen.insert(m.name.clone()));
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Microagents whose triggers match `prompt` (case-insensitive substring,
/// or the `always` trigger). Empty when there is no microagents root.
pub fn matching(cwd: &Path, prompt: &str) -> Vec<Microagent> {
    let lower = prompt.to_lowercase();
    scan(cwd)
        .into_iter()
        .filter(|m| {
            m.triggers.iter().any(|t| {
                let t = t.to_lowercase();
                t == ALWAYS_TRIGGER || (!t.is_empty() && lower.contains(&t))
            })
        })
        .collect()
}

/// Provenance-wrapped rendering of the matching bodies — what the agent
/// injects as a Nudge. Wrapped like a skill body so repo-authored
/// instructions can never pose as user/system text.
pub fn render(agents: &[Microagent]) -> String {
    let mut out = String::new();
    for a in agents {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!(
            "<microagent name=\"{}\" path=\"{}\">\n{}\n</microagent>",
            a.name,
            a.path.display(),
            a.body
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-micro-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mk(root: &Path, rel: &str, fm: &str, body: &str) {
        let d = root.join(".overseer/microagents").join(rel);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(MICROAGENT_FILE), format!("---\n{fm}---\n{body}")).unwrap();
    }

    #[test]
    fn scan_walks_nested_dirs_sorted() {
        let dir = tmpdir();
        mk(&dir, "zeta", "name: zeta\ntriggers: z\n", "Z body");
        mk(&dir, "nested/deep", "name: alpha\ntriggers: a\n", "A body");
        let found = scan(&dir);
        assert_eq!(
            found.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["alpha", "zeta"],
            "nested entries are discovered and the list is name-sorted"
        );
        assert!(found[0]
            .path
            .ends_with(".overseer/microagents/nested/deep/MICROAGENT.md"));
        assert_eq!(found[0].body, "A body");
    }

    #[test]
    fn triggers_route_prompts_case_insensitively() {
        let dir = tmpdir();
        mk(
            &dir,
            "style",
            "name: style\ntriggers: unwrap, clippy\n",
            "no unwraps",
        );
        mk(
            &dir,
            "always",
            "name: always\ntriggers: always\n",
            "be brief",
        );
        let hits = matching(&dir, "Please fix the CLIPPY warnings");
        assert_eq!(
            hits.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(),
            ["always", "style"],
            "substring + always triggers both route"
        );
        // Unrelated prompt → only the always-on agent.
        let hits = matching(&dir, "add a docstring");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "always");
    }

    #[test]
    fn rendered_body_is_provenance_wrapped_and_metadata_free() {
        let dir = tmpdir();
        mk(&dir, "x", "name: x\ntriggers: alpha\n", "body text here");
        let hits = matching(&dir, "alpha task");
        assert_eq!(hits.len(), 1);
        let out = render(&hits);
        assert!(out.starts_with("<microagent name=\"x\""));
        assert!(out.contains("body text here"));
        assert!(out.ends_with("</microagent>"), "{out}");
        // Frontmatter is metadata, not body.
        assert!(!out.contains("triggers:"));
        // No root → nothing to route.
        assert!(matching(&tmpdir(), "alpha").is_empty());
    }
}
