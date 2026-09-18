//! Session modes (roo-code custom modes, arsenal B2).
//!
//! A mode is a named working posture: which tools are available, which
//! model runs, which files it may edit, and a short line of framing that
//! rides the conversation. Roo ships five built-ins (Code/Architect/Ask/
//! Debug/Orchestrator) with per-mode tool groups; this port keeps the
//! shape and the two enforcement points that matter to a harness:
//!
//! 1. **Toolset** — `ToolRegistry::set_mode` removes every tool the mode
//!    does not allow (capability removal, exactly like plan mode and
//!    `--no-tools`, so the spec list and dispatch agree).
//! 2. **Edit globs** — `edit`/`write` refuse a path outside the mode's
//!    `edit_globs` (empty = unrestricted). The check lives in the file
//!    tools so it cannot be bypassed by a caller that skips the CLI.
//!
//! `prompt_frag` is delivered as a harness-authored `Nudge` at mode
//! switch, not spliced into the static prompt: the frozen ORDER segments
//! stay byte-stable, and a logged Nudge replays identically on resume.
//! `// DEFERRED(owner): user-defined modes (roo's `.roomodes` file) —
//! built-ins only; the loader would be a sibling of `recipe::load`.`

use crate::provider::ToolSpec;

/// One named working posture.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    /// Mode name as addressed by `--mode`/`for_mode`.
    pub name: &'static str,
    /// One line of framing injected as a Nudge when the mode is set.
    pub prompt_frag: &'static str,
    /// Tools available in this mode; every other resident tool is removed.
    pub allowed_tools: &'static [&'static str],
    /// Model override; `None` keeps the session's model.
    pub model: Option<&'static str>,
    /// Glob patterns the file tools may touch; empty = unrestricted.
    pub edit_globs: &'static [&'static str],
}

impl Mode {
    /// Whether this mode keeps `tool` resident.
    pub fn allows(&self, tool: &str) -> bool {
        self.allowed_tools.contains(&tool)
    }

    /// Tool names to hand to `ToolRegistry::disable` for this mode.
    pub fn disable_list(&self) -> Vec<String> {
        crate::tools::TOOL_NAMES
            .iter()
            .filter(|n| !self.allows(n))
            .map(|n| (*n).to_string())
            .collect()
    }

    /// Names to hand to `ToolRegistry::disable` given the specs actually
    /// resident (a mode never resurrects a tool the caller ablated).
    pub fn disable_list_for(&self, specs: &[ToolSpec]) -> Vec<String> {
        specs
            .iter()
            .map(|s| s.name.clone())
            .filter(|n| !self.allows(n))
            .collect()
    }

    /// Whether `path` (as spelled by the model, relative to the workspace)
    /// is inside this mode's edit globs. Empty globs allow everything.
    pub fn edit_allowed(&self, path: &str) -> bool {
        if self.edit_globs.is_empty() {
            return true;
        }
        let normalized = path.replace('\\', "/");
        self.edit_globs.iter().any(|g| {
            globset::GlobBuilder::new(g)
                .literal_separator(false)
                .build()
                .map(|c| c.compile_matcher().is_match(&normalized))
                .unwrap_or(false)
        })
    }
}

/// The built-in modes. `code` is the default posture (every tool).
pub static MODES: &[Mode] = &[
    Mode {
        name: "code",
        prompt_frag: "Implement and verify changes in this repository.",
        allowed_tools: &crate::tools::TOOL_NAMES,
        model: None,
        edit_globs: &[],
    },
    Mode {
        name: "architect",
        prompt_frag: "Plan before acting; do not modify files in this mode.",
        allowed_tools: &[
            "read", "grep", "glob", "plan", "task", "skill", "repo_map", "symbol",
        ],
        model: None,
        edit_globs: &[],
    },
    Mode {
        name: "ask",
        prompt_frag: "Answer questions about this repository; make no changes.",
        allowed_tools: &["read", "grep", "glob", "skill", "repo_map", "symbol"],
        model: None,
        edit_globs: &[],
    },
    Mode {
        name: "debug",
        prompt_frag:
            "Reproduce the failure first, then fix it; keep the repro as a regression test.",
        allowed_tools: &[
            "read", "grep", "glob", "bash", "edit", "write", "plan", "task", "skill", "repo_map",
            "symbol",
        ],
        model: None,
        edit_globs: &[],
    },
    Mode {
        name: "docs",
        prompt_frag: "Edit documentation only; leave code untouched.",
        allowed_tools: &["read", "grep", "glob", "edit", "write", "skill", "repo_map"],
        model: None,
        edit_globs: &["**/*.md", "**/*.mdx", "**/*.txt"],
    },
];

/// Resolve a mode by name (exact match, case-insensitive).
pub fn for_mode(name: &str) -> Option<&'static Mode> {
    let n = name.trim().to_ascii_lowercase();
    MODES.iter().find(|m| m.name == n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_modes_have_disjoint_expected_toolsets() {
        let architect = for_mode("architect").unwrap();
        assert!(!architect.allows("write"));
        assert!(!architect.allows("edit"));
        assert!(!architect.allows("bash"));
        assert!(architect.allows("read") && architect.allows("plan"));
        let disabled = architect.disable_list();
        for t in ["write", "edit", "bash", "computer"] {
            assert!(disabled.iter().any(|d| d == t), "{t} must be disabled");
        }
        // `code` is the full resident set — nothing removed.
        assert!(for_mode("code").unwrap().disable_list().is_empty());
        // Unknown mode → None (the caller reports the known names).
        assert!(for_mode("nope").is_none());
        assert_eq!(for_mode("  Architect ").unwrap().name, "architect");
    }

    #[test]
    fn mode_never_resurrects_an_ablated_tool() {
        // disable_list_for works off the resident specs, so a caller that
        // already dropped `bash` can't have it re-advertised by a mode.
        let specs = vec![ToolSpec {
            name: "read".into(),
            description: String::new(),
            input_schema: serde_json::json!({}),
        }];
        let docs = for_mode("docs").unwrap();
        let disabled = docs.disable_list_for(&specs);
        assert!(disabled.is_empty(), "read is allowed by docs mode");
        let architect = for_mode("architect").unwrap();
        assert!(architect.disable_list_for(&specs).is_empty());
    }

    #[test]
    fn edit_globs_bound_the_file_tools() {
        let docs = for_mode("docs").unwrap();
        assert!(docs.edit_allowed("README.md"));
        assert!(docs.edit_allowed("docs/guide/readme.mdx"));
        assert!(!docs.edit_allowed("src/main.rs"));
        // Empty globs = unrestricted (code mode and any file in architect,
        // which is moot there because the tools are removed).
        assert!(for_mode("code").unwrap().edit_allowed("src/main.rs"));
        // A glob that does not parse fails closed.
        let bad = Mode {
            edit_globs: &["["],
            ..*docs
        };
        assert!(!bad.edit_allowed("README.md"));
    }
}
