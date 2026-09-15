//! SKILL.md progressive disclosure (playbook Ch.3 §9.7, P3.5).
//!
//! Skills are directories with a `SKILL.md`: YAML-ish frontmatter
//! (`name`, `description`) + a markdown body. Only the *metadata* is
//! resident in the prompt (~one line per skill, ≲100 tokens each); the
//! body loads on demand through the `skill` tool — the earn-your-tokens
//! rule applied to capabilities.
//!
//! Roots scanned (first hit wins on name collision, workspace first):
//!   `<cwd>/.overseer/skills/<name>/SKILL.md`   — project skills
//!   `~/.overseer/skills/<name>/SKILL.md`       — user skills
//!
//! Loaded bodies are provenance-wrapped (`<skill name=… source=…>`) so
//! remote-originated instructions are never confused with user/system
//! text (P3.10 provenance marking).

use std::path::{Path, PathBuf};

/// One discovered skill's resident metadata.
#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    /// Absolute path to the SKILL.md (its dir holds support files).
    pub path: PathBuf,
    /// Where it came from: `workspace` or `user` (provenance label).
    pub source: &'static str,
}

/// `---`-delimited frontmatter → (name, description). Deliberately
/// minimal — `key: value` lines only, no YAML dep for two fields.
fn parse_frontmatter(text: &str) -> Option<(String, String)> {
    let t = text.strip_prefix("---")?;
    let fm = t.split("\n---").next()?;
    let mut name = None;
    let mut desc = None;
    for line in fm.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            match k.trim() {
                "name" => name = Some(v.to_string()),
                "description" => desc = Some(v.to_string()),
                _ => {}
            }
        }
    }
    Some((name?, desc.unwrap_or_default()))
}

/// The body after the frontmatter block (what `skill` loads on demand).
pub fn body_of(text: &str) -> String {
    text.splitn(3, "---")
        .nth(2)
        .unwrap_or(text)
        .trim()
        .to_string()
}

/// Scan one skills root; returns metas for every `*/SKILL.md` with a
/// parseable name. Sorted by name — deterministic prompt order.
fn scan_root(root: &Path, source: &'static str) -> Vec<SkillMeta> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let md = e.path().join("SKILL.md");
            let Ok(text) = std::fs::read_to_string(&md) else {
                continue;
            };
            if let Some((name, description)) = parse_frontmatter(&text) {
                out.push(SkillMeta {
                    name,
                    description,
                    path: md,
                    source,
                });
            }
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// All skills under the standard roots (workspace first).
pub fn scan(cwd: &Path) -> Vec<SkillMeta> {
    let mut out = scan_root(&cwd.join(".overseer/skills"), "workspace");
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/"));
    out.extend(scan_root(&home.join(".overseer/skills"), "user"));
    // Workspace wins on name collisions (first hit rule).
    let mut seen = std::collections::HashSet::new();
    out.retain(|s| seen.insert(s.name.clone()));
    out
}

/// Resident metadata segment (~one line per skill). Sits in the static
/// region after the memory index — skill installs are rare, so a change
/// here only invalidates cache from this segment onward.
pub fn index_segment(cwd: &Path) -> Option<String> {
    let skills = scan(cwd);
    if skills.is_empty() {
        return None;
    }
    let mut lines = String::from(
        "## Skills\nLoad a skill's full instructions with the `skill` tool when \
         its description matches the task. One line per skill:\n",
    );
    for s in &skills {
        lines.push_str(&format!("- {} — {}\n", s.name, s.description));
    }
    Some(lines)
}

/// Load a skill body by name, provenance-wrapped. `Err` names the
/// available skills so the model can self-correct.
pub fn load(cwd: &Path, name: &str) -> Result<String, String> {
    let skills = scan(cwd);
    let Some(s) = skills.iter().find(|s| s.name == name) else {
        let avail: Vec<&str> = skills.iter().map(|s| s.name.as_str()).collect();
        return Err(format!(
            "no skill '{name}'. Available: {}",
            if avail.is_empty() {
                "(none)".into()
            } else {
                avail.join(", ")
            }
        ));
    };
    let text = std::fs::read_to_string(&s.path).map_err(|e| e.to_string())?;
    Ok(format!(
        "<skill name=\"{}\" source=\"{}\" path=\"{}\">\n{}\n</skill>",
        s.name,
        s.source,
        s.path.display(),
        body_of(&text)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-skill-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn mk_skill(root: &Path, dir: &str, fm: &str, body: &str) {
        let d = root.join(dir);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("SKILL.md"), format!("---\n{fm}---\n{body}")).unwrap();
    }

    #[test]
    fn scan_finds_workspace_skills_sorted() {
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(&root, "beta", "name: beta\ndescription: second\n", "B body");
        mk_skill(&root, "alpha", "name: alpha\ndescription: first\n", "A body");
        let skills = scan(&dir);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[0].source, "workspace");
    }

    #[test]
    fn body_loads_with_provenance() {
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(&root, "x", "name: x\ndescription: does x\n", "do the x thing");
        let out = load(&dir, "x").unwrap();
        assert!(out.contains("<skill name=\"x\" source=\"workspace\""));
        assert!(out.contains("do the x thing"));
        assert!(out.contains("</skill>"));
        assert!(load(&dir, "missing").unwrap_err().contains("x"));
    }

    #[test]
    fn index_segment_is_metadata_only() {
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        let big = "x".repeat(50_000);
        mk_skill(&root, "fat", "name: fat\ndescription: heavy skill\n", &big);
        let seg = index_segment(&dir).unwrap();
        assert!(seg.contains("fat — heavy skill"));
        assert!(seg.len() < 1_000, "body must not be resident");
        assert!(index_segment(&tmpdir()).is_none(), "empty → no segment");
    }
}
