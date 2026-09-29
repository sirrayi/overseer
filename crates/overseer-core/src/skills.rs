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
    /// `trigger:`/`triggers:` phrases (awesomeclaude convention, B2): a
    /// resident routing hint. Trigger *lines* are rendered in the index
    /// (routing precision for the model); `matching()` gives the engine the
    /// same signal for lookup-miss hints.
    pub triggers: Vec<String>,
}

/// Split `---`-delimited frontmatter from the body: `(front, body)`. A
/// delimiter is a line that is exactly `---` (trailing whitespace allowed),
/// so a `---` inside a value never ends the block. `None` when the text
/// does not open with a delimiter or the block is unterminated. Shared by
/// skills and microagents.
pub(crate) fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let mut lines = text.split_inclusive('\n');
    let first = lines.next()?;
    if first.trim_end() != "---" {
        return None;
    }
    let start = first.len();
    let mut pos = start;
    for line in lines {
        if line.trim_end() == "---" {
            return Some((&text[start..pos], &text[pos + line.len()..]));
        }
        pos += line.len();
    }
    None
}

/// `---`-delimited frontmatter → (name, description, triggers).
/// Deliberately minimal — `key: value` lines only, no YAML dep.
fn parse_frontmatter(text: &str) -> Option<(String, String, Vec<String>)> {
    let (fm, _) = split_frontmatter(text)?;
    let mut name = None;
    let mut desc = None;
    let mut triggers: Vec<String> = Vec::new();
    for line in fm.lines() {
        if let Some((k, v)) = line.split_once(':') {
            let v = v.trim().trim_matches('"').trim_matches('\'');
            match k.trim() {
                "name" => name = Some(v.to_string()),
                "description" => desc = Some(v.to_string()),
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
    }
    triggers.sort();
    triggers.dedup();
    Some((name?, desc.unwrap_or_default(), triggers))
}

/// The body after the frontmatter block (what `skill` loads on demand).
pub fn body_of(text: &str) -> String {
    split_frontmatter(text)
        .map_or(text, |(_, body)| body)
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
            if let Some((name, description, triggers)) = parse_frontmatter(&text) {
                out.push(SkillMeta {
                    name,
                    description,
                    path: md,
                    source,
                    triggers,
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
        // Trigger phrases ride the resident line (awesomeclaude `trigger:`
        // convention): one extra clause per skill, and the model routes on
        // it without loading the body — the same earn-your-tokens trade
        // the description already makes.
        if s.triggers.is_empty() {
            lines.push_str(&format!("- {} — {}\n", s.name, s.description));
        } else {
            lines.push_str(&format!(
                "- {} — {} [triggers: {}]\n",
                s.name,
                s.description,
                s.triggers.join(", ")
            ));
        }
    }
    Some(lines)
}

/// Skills whose triggers match `prompt` (case-insensitive substring), most
/// specific first: more matched triggers, then name order. Empty when no
/// skill declares a trigger that hits — a skill without triggers is never
/// a "match", it is only reachable by description.
pub fn matching(cwd: &Path, prompt: &str) -> Vec<SkillMeta> {
    let lower = prompt.to_lowercase();
    let mut hits: Vec<(usize, SkillMeta)> = scan(cwd)
        .into_iter()
        .filter_map(|s| {
            let n = s
                .triggers
                .iter()
                .filter(|t| {
                    let t = t.to_lowercase();
                    !t.is_empty() && lower.contains(&t)
                })
                .count();
            (n > 0).then_some((n, s))
        })
        .collect();
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
    hits.into_iter().map(|(_, s)| s).collect()
}

/// Tokenize for overlap scoring: lowercase alphanumeric tokens.
fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Score one skill against a query: exact name = 1.0, trigger substring =
/// 0.8, else the fraction of distinct query tokens present in the skill's
/// name/description/triggers (0.0 when the query has no tokens).
fn score_skill(s: &SkillMeta, query: &str) -> f32 {
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return 0.0;
    }
    if s.name.to_lowercase() == q {
        return 1.0;
    }
    if s.triggers.iter().any(|t| {
        let t = t.to_lowercase();
        !t.is_empty() && q.contains(&t)
    }) {
        return 0.8;
    }
    let mut qtok = tokens(&q);
    qtok.sort();
    qtok.dedup();
    if qtok.is_empty() {
        return 0.0;
    }
    let stext = format!("{} {} {}", s.name, s.description, s.triggers.join(" ")).to_lowercase();
    let stokens: std::collections::BTreeSet<String> = tokens(&stext).into_iter().collect();
    let hits = qtok.iter().filter(|t| stokens.contains(*t)).count();
    hits as f32 / qtok.len() as f32
}

/// Rank all skills against `query` (exact name, then trigger substring, then
/// token overlap), score-descending with a name-ascending tiebreak, capped
/// at `top_k` (`top_k = 0` returns empty). Zero-scored skills are dropped.
pub fn search_ranked(cwd: &Path, query: &str, top_k: usize) -> Vec<(SkillMeta, f32)> {
    if top_k == 0 {
        return Vec::new();
    }
    let mut ranked: Vec<(SkillMeta, f32)> = scan(cwd)
        .into_iter()
        .map(|s| {
            let score = score_skill(&s, query);
            (s, score)
        })
        .filter(|(_, score)| *score > 0.0)
        .collect();
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.name.cmp(&b.0.name))
    });
    ranked.truncate(top_k);
    ranked
}

/// Minimum score for query-driven autoload.
pub const AUTOLOAD_THRESHOLD: f32 = 0.8;

/// Skills worth loading for `query` without asking: everything at or above
/// [`AUTOLOAD_THRESHOLD`], in [`search_ranked`] order.
pub fn autoload(cwd: &Path, query: &str) -> Vec<SkillMeta> {
    search_ranked(cwd, query, usize::MAX)
        .into_iter()
        .filter(|(_, score)| *score >= AUTOLOAD_THRESHOLD)
        .map(|(s, _)| s)
        .collect()
}

/// One-line multi-skill reminder: names the candidates and points at the
/// `skill` tool, or reports that nothing matched.
pub fn reminder_line(names: &[&str]) -> String {
    if names.is_empty() {
        return "No skills matched.".to_string();
    }
    format!(
        "Available skills: {} — load via skill tool; bodies on demand.",
        names.join(", ")
    )
}

/// Load a skill body by name, provenance-wrapped. `Err` names the
/// available skills so the model can self-correct.
pub fn load(cwd: &Path, name: &str) -> Result<String, String> {
    let skills = scan(cwd);
    let Some(s) = skills.iter().find(|s| s.name == name) else {
        let names: Vec<String> = skills.iter().map(|s| s.name.clone()).collect();
        let hint = crate::fuzzy::miss_hint(name, &names, 3)
            .map(|h| format!(" {h}"))
            .unwrap_or_default();
        return Err(format!(
            "no skill '{name}'. Available: {}{hint}",
            if names.is_empty() {
                "(none)".into()
            } else {
                names.join(", ")
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
    fn frontmatter_delimiters_are_whole_lines() {
        let text = "---\nname: dash\ndescription: a---b\n---  \nbody line\n\n---\nafter rule\n";
        let (name, desc, _) = parse_frontmatter(text).unwrap();
        assert_eq!(name, "dash");
        assert_eq!(desc, "a---b");
        assert_eq!(body_of(text), "body line\n\n---\nafter rule");
        assert_eq!(
            split_frontmatter(text),
            Some((
                "name: dash\ndescription: a---b\n",
                "body line\n\n---\nafter rule\n"
            ))
        );
        // `----` is not a delimiter; an unterminated block is no frontmatter.
        assert!(split_frontmatter("----\nname: x\n---\nb").is_none());
        assert!(split_frontmatter("---\nname: x\n").is_none());
        assert_eq!(body_of("# plain\ntext"), "# plain\ntext");
    }

    #[test]
    fn scan_finds_workspace_skills_sorted() {
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(&root, "beta", "name: beta\ndescription: second\n", "B body");
        mk_skill(
            &root,
            "alpha",
            "name: alpha\ndescription: first\n",
            "A body",
        );
        let skills = scan(&dir);
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "alpha");
        assert_eq!(skills[0].source, "workspace");
    }

    #[test]
    fn body_loads_with_provenance() {
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(
            &root,
            "x",
            "name: x\ndescription: does x\n",
            "do the x thing",
        );
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

    #[test]
    fn miss_hints_rank_near_names_and_stay_quiet_otherwise() {
        // P8-B accept (fzf lookup-miss hints): a name that is a subsequence
        // of a real skill gets a ranked suggestion; an unrelated one gets
        // only the available list (no noise).
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(&root, "deploy", "name: deploy\ndescription: ship it\n", "B");
        mk_skill(&root, "deps", "name: deps\ndescription: list deps\n", "B");
        let near = load(&dir, "dpl").unwrap_err();
        assert!(near.contains("did you mean:"), "{near}");
        assert!(near.contains("deploy"), "{near}");
        let far = load(&dir, "qqqq").unwrap_err();
        assert!(!far.contains("did you mean:"), "no noise hints: {far}");
        assert!(far.contains("Available:"), "{far}");
        assert!(far.contains("deploy"), "{far}");
        assert!(far.contains("deps"), "{far}");
    }

    #[test]
    fn trigger_lines_route_without_loading_bodies() {
        // P8-B accept (awesomeclaude `trigger:` frontmatter): trigger
        // phrases are resident routing hints — indexed, and matchable
        // engine-side for the same prompt.
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(
            &root,
            "pdf",
            "name: pdf\ndescription: fill forms\ntrigger: pdf, acroform\n",
            "SECRET-BODY",
        );
        mk_skill(
            &root,
            "plain",
            "name: plain\ndescription: no triggers\n",
            "B",
        );
        let seg = index_segment(&dir).unwrap();
        assert!(
            seg.contains("pdf — fill forms [triggers: acroform, pdf]"),
            "{seg}"
        );
        // The plain skill's line is unchanged — no empty bracket.
        assert!(seg.contains("- plain — no triggers\n"), "{seg}");
        assert!(!seg.contains("SECRET-BODY"), "bodies stay out of the index");

        let hits = matching(&dir, "please fill this ACROFORM for me");
        assert_eq!(
            hits.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["pdf"],
            "a trigger hit routes the skill"
        );
        assert!(matching(&dir, "unrelated request").is_empty());
        // Most-specific first: a skill matching two triggers outranks one.
        mk_skill(
            &root,
            "both",
            "name: both\ndescription: two hits\ntriggers: acroform, pdf\n",
            "B",
        );
        let hits = matching(&dir, "acroform pdf work");
        assert_eq!(
            hits[0].name,
            "both",
            "{:?}",
            hits.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn ranked_search_orders_exact_then_trigger_then_overlap() {
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(
            &root,
            "pdf",
            "name: pdf\ndescription: fill forms\ntrigger: acroform\n",
            "B",
        );
        mk_skill(
            &root,
            "alpha",
            "name: alpha\ndescription: fill forms\n",
            "B",
        );
        // Exact name hit scores 1.0 and leads.
        let ranked = search_ranked(&dir, "pdf", 10);
        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].0.name, "pdf");
        assert_eq!(ranked[0].1, 1.0);
        // Trigger hit (0.8) with no token-overlap competition.
        let ranked = search_ranked(&dir, "acroform", 10);
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].0.name, "pdf");
        assert_eq!(ranked[0].1, 0.8);
        // Equal overlap scores break ties by name.
        let ranked = search_ranked(&dir, "fill forms", 10);
        let names: Vec<&str> = ranked.iter().map(|(s, _)| s.name.as_str()).collect();
        assert_eq!(names, ["alpha", "pdf"]);
        // top_k caps; 0 → empty; unknown query → empty.
        assert_eq!(search_ranked(&dir, "fill forms", 1).len(), 1);
        assert!(search_ranked(&dir, "pdf", 0).is_empty());
        assert!(search_ranked(&dir, "zzzqqq", 10).is_empty());
    }

    #[test]
    fn autoload_threshold_and_reminder_line() {
        let dir = tmpdir();
        let root = dir.join(".overseer/skills");
        mk_skill(
            &root,
            "pdf",
            "name: pdf\ndescription: fill forms\ntrigger: acroform\n",
            "B",
        );
        mk_skill(
            &root,
            "unrelated",
            "name: unrelated\ndescription: juggling\n",
            "B",
        );
        assert_eq!(AUTOLOAD_THRESHOLD, 0.8);
        let loaded = autoload(&dir, "acroform work");
        assert_eq!(
            loaded.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["pdf"]
        );
        // Pure token overlap (0.25) stays below the threshold.
        assert!(autoload(&dir, "juggling tips and tricks").is_empty());
        assert_eq!(
            reminder_line(&["alpha", "pdf"]),
            "Available skills: alpha, pdf — load via skill tool; bodies on demand."
        );
        assert_eq!(reminder_line(&[]), "No skills matched.");
    }
}
