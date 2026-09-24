//! Recipe files (goose recipe pattern, arsenal B2).
//!
//! A recipe is a repo-committed "how to work here" declaration: which
//! tools a session may use and which model it should run. Goose stores it
//! as a YAML file with `instructions`/`prompt`/`extensions`; this port
//! keeps the two fields that are *enforceable* by the engine and drops the
//! prose (prose belongs in AGENTS.md, which the model already reads).
//!
//! File: `<cwd>/RECIPE.md`, frontmatter only:
//!
//! ```markdown
//! ---
//! allowed_tools: read, grep, glob, edit
//! model: deepseek-v4.1-flash
//! ---
//! ```
//!
//! Enforcement is capability removal, not denial: `disable_list()` returns
//! every `TOOL_NAMES` entry the recipe does *not* allow, which the caller
//! hands to `ToolRegistry::disable` — the same mechanism `--no-tools` uses,
//! so an ablated tool is absent from the spec list *and* refused at
//! dispatch. A tool name outside `TOOL_NAMES` is a load error (a typo must
//! fail loudly, not silently widen the set).
//!
//! `allowed_tools` is required; `model` is optional (absent = keep the
//! caller's model). An absent RECIPE.md is `Ok(None)` — recipes are opt-in.
//! `// DEFERRED(owner): recipe instructions/params (goose's prose +
//! templating) — prose stays in AGENTS.md; wire the module into the CLI
//! once operators commit a RECIPE.md.`

use std::path::{Path, PathBuf};

/// The recipe file's name in the session cwd.
pub const RECIPE_FILE: &str = "RECIPE.md";

/// A parsed recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub path: PathBuf,
    /// Tool names the session may use (validated against `TOOL_NAMES`).
    pub allowed_tools: Vec<String>,
    /// Model override; `None` keeps the caller's model.
    pub model: Option<String>,
}

impl Recipe {
    /// Names to hand to `ToolRegistry::disable`: every resident tool the
    /// recipe does not allow, in registry order.
    pub fn disable_list(&self) -> Vec<String> {
        crate::tools::TOOL_NAMES
            .iter()
            .filter(|n| !self.allowed_tools.iter().any(|a| a == *n))
            .map(|n| (*n).to_string())
            .collect()
    }
}

/// Parse the frontmatter of `text` into a recipe. Errors name the fault
/// (unterminated frontmatter, unknown tool, empty allowed_tools).
pub fn parse(path: &Path, text: &str) -> Result<Recipe, String> {
    let Some(rest) = text.strip_prefix("---") else {
        return Err(format!(
            "{RECIPE_FILE}: missing frontmatter — start the file with `---`"
        ));
    };
    let Some(fm) = rest.split("\n---").next() else {
        return Err(format!("{RECIPE_FILE}: unterminated frontmatter"));
    };
    let mut allowed: Vec<String> = Vec::new();
    let mut model: Option<String> = None;
    let mut saw_allowed = false;
    for line in fm.lines() {
        let Some((k, v)) = line.split_once(':') else {
            continue;
        };
        let v = v.trim().trim_matches('"').trim_matches('\'');
        match k.trim() {
            "allowed_tools" => {
                saw_allowed = true;
                for part in v.split(',') {
                    let p = part.trim();
                    if !p.is_empty() {
                        allowed.push(p.to_string());
                    }
                }
            }
            "model" if !v.is_empty() => model = Some(v.to_string()),
            _ => {}
        }
    }
    if !saw_allowed {
        return Err(format!(
            "{RECIPE_FILE}: `allowed_tools` is required (comma-separated tool names)"
        ));
    }
    if allowed.is_empty() {
        return Err(format!(
            "{RECIPE_FILE}: `allowed_tools` is empty — list the tools this repo allows \
             (use `read` alone for a read-only recipe)"
        ));
    }
    for t in &allowed {
        if !crate::tools::TOOL_NAMES.contains(&t.as_str()) {
            return Err(format!(
                "{RECIPE_FILE}: unknown tool `{t}` — valid names: {}",
                crate::tools::TOOL_NAMES.join(", ")
            ));
        }
    }
    allowed.sort();
    allowed.dedup();
    Ok(Recipe {
        path: path.to_path_buf(),
        allowed_tools: allowed,
        model,
    })
}

/// Load `<cwd>/RECIPE.md`. Absent file → `Ok(None)`; unreadable file is
/// reported (an unreadable recipe must not silently run unrestricted).
pub fn load(cwd: &Path) -> Result<Option<Recipe>, String> {
    let path = cwd.join(RECIPE_FILE);
    match std::fs::read_to_string(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "{RECIPE_FILE}: cannot read {} — {e}",
            path.display()
        )),
        Ok(text) => parse(&path, &text).map(Some),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-recipe-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn disable_list_is_the_complement_of_allowed_tools() {
        let dir = tmpdir();
        std::fs::write(
            dir.join(RECIPE_FILE),
            "---\nallowed_tools: read, grep, glob\nmodel: deepseek-v4.1-flash\n---\nnotes\n",
        )
        .unwrap();
        let r = load(&dir).unwrap().expect("recipe present");
        assert_eq!(r.model.as_deref(), Some("deepseek-v4.1-flash"));
        let disabled = r.disable_list();
        assert!(!disabled.iter().any(|d| d == "read"));
        assert!(!disabled.iter().any(|d| d == "grep"));
        for tool in ["write", "edit", "bash", "task"] {
            assert!(
                disabled.iter().any(|d| d == tool),
                "{tool} must be disabled by the recipe"
            );
        }
        // Complement property: allowed ∪ disabled == the resident set.
        assert_eq!(disabled.len(), crate::tools::TOOL_NAMES.len() - 3);
    }

    #[test]
    fn load_is_opt_in_and_typos_fail_loudly() {
        let dir = tmpdir();
        assert!(load(&dir).unwrap().is_none(), "no file → no recipe");

        std::fs::write(dir.join(RECIPE_FILE), "---\nmodel: x\n---\n").unwrap();
        let err = load(&dir).unwrap_err();
        assert!(err.contains("allowed_tools"), "{err}");

        std::fs::write(dir.join(RECIPE_FILE), "---\nallowed_tools: readd\n---\n").unwrap();
        let err = load(&dir).unwrap_err();
        assert!(err.contains("unknown tool `readd`"), "{err}");
        assert!(err.contains("read"), "the error lists valid names: {err}");

        std::fs::write(dir.join(RECIPE_FILE), "allowed_tools: read\n").unwrap();
        assert!(
            load(&dir).unwrap_err().contains("frontmatter"),
            "a recipe without frontmatter is rejected, not silently empty"
        );
    }
}
