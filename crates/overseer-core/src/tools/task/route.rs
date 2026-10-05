//! Mode → registry and tier → model routing for one spawn.

use serde::{Deserialize, Serialize};

use crate::agent::AgentConfig;
use crate::profile::{self, Tier};
use crate::provider::Effort;
use crate::tools::ToolRegistry;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskMode {
    Read,
    Write,
    Verify,
    Consult,
}

impl TaskMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "read" => Some(TaskMode::Read),
            "write" => Some(TaskMode::Write),
            "verify" => Some(TaskMode::Verify),
            "consult" => Some(TaskMode::Consult),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskMode::Read => "read",
            TaskMode::Write => "write",
            TaskMode::Verify => "verify",
            TaskMode::Consult => "consult",
        }
    }

    /// The tier a spawn gets without an explicit `tier`. Never light for a
    /// writer: a cheap writer must be asked for.
    pub fn default_tier(self) -> Tier {
        match self {
            TaskMode::Read => Tier::Light,
            TaskMode::Write | TaskMode::Verify => Tier::Standard,
            TaskMode::Consult => Tier::Heavy,
        }
    }

    /// Only read and consult retry one tier up; a writer's partial edits
    /// must not be silently redone.
    pub fn escalates(self) -> bool {
        matches!(self, TaskMode::Read | TaskMode::Consult)
    }

    /// The registry a subagent of this mode runs with — at spawn and again
    /// on resume, so a continuation never gains tools. Consult has no loop.
    /// Writers detect skills under `cwd` (their worktree).
    pub fn registry(self, policy: crate::perm::Policy, cwd: &std::path::Path) -> ToolRegistry {
        match self {
            // DEFERRED(owner): read-class deferred tools (repo_map/symbol via tools) in read subagents — gate: demand
            TaskMode::Read | TaskMode::Consult => ToolRegistry::readonly(policy),
            TaskMode::Write => ToolRegistry::core_in(policy, cwd),
            TaskMode::Verify => ToolRegistry::readonly_with_bash(policy),
        }
    }
}

/// What one attempt runs on.
#[derive(Debug, Clone, PartialEq)]
pub struct Route {
    pub tier: Tier,
    pub model: String,
    pub effort: Option<Effort>,
    /// Why the tier fell back to the parent's model, when it did.
    pub note: Option<String>,
}

/// light = `small_model`, else the family's cheapest row; standard = the
/// parent's model; heavy = `heavy_model`, else the family's priciest row.
/// A candidate that needs another transport than the parent's model is
/// dropped for the parent's model — one provider instance serves all.
pub fn resolve(tier: Tier, parent: &AgentConfig) -> Route {
    let family = profile::lookup(&parent.model).family;
    let candidate = match tier {
        Tier::Light => parent
            .small_model
            .clone()
            .or_else(|| profile::tier_model(family, Tier::Light).map(str::to_string)),
        Tier::Standard => None,
        Tier::Heavy => parent
            .heavy_model
            .clone()
            .or_else(|| profile::tier_model(family, Tier::Heavy).map(str::to_string)),
    };
    let (model, note) = match candidate {
        Some(m) if profile::same_transport(&parent.model, &m) => (m, None),
        Some(m) => (
            parent.model.clone(),
            Some(format!(
                "{m} needs another transport; using {}",
                parent.model
            )),
        ),
        None => (parent.model.clone(), None),
    };
    let effort = match tier {
        Tier::Light => Some(Effort::Low),
        Tier::Standard => parent.effort,
        Tier::Heavy => Some(Effort::High),
    };
    Route {
        tier,
        model,
        effort,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(model: &str) -> AgentConfig {
        AgentConfig {
            model: model.into(),
            effort: Some(Effort::Medium),
            ..AgentConfig::default()
        }
    }

    #[test]
    fn anthropic_tiers_resolve_from_the_table() {
        let c = cfg("claude-sonnet-5");
        let l = resolve(Tier::Light, &c);
        assert_eq!(
            (l.model.as_str(), l.effort),
            ("claude-haiku-4-5", Some(Effort::Low))
        );
        let s = resolve(Tier::Standard, &c);
        assert_eq!(
            (s.model.as_str(), s.effort),
            ("claude-sonnet-5", Some(Effort::Medium))
        );
        let h = resolve(Tier::Heavy, &c);
        assert_eq!(
            (h.model.as_str(), h.effort),
            ("claude-fable-5", Some(Effort::High))
        );
        assert!(l.note.is_none() && h.note.is_none());
    }

    #[test]
    fn flags_win_but_never_cross_transport() {
        let mut c = cfg("claude-sonnet-5");
        c.small_model = Some("claude-sonnet-4-5".into());
        c.heavy_model = Some("deepseek-v4.1-flash".into());
        assert_eq!(resolve(Tier::Light, &c).model, "claude-sonnet-4-5");
        let h = resolve(Tier::Heavy, &c);
        assert_eq!(h.model, "claude-sonnet-5");
        assert!(h.note.unwrap().contains("deepseek-v4.1-flash"));
        // Single-row families fall back to the parent model silently.
        let d = cfg("deepseek-v4.1-flash");
        assert_eq!(resolve(Tier::Heavy, &d).model, "deepseek-v4.1-flash");
        assert_eq!(resolve(Tier::Light, &d).note, None);
    }

    #[test]
    fn default_tier_by_mode() {
        assert_eq!(TaskMode::Read.default_tier(), Tier::Light);
        assert_eq!(TaskMode::Write.default_tier(), Tier::Standard);
        assert_eq!(TaskMode::Verify.default_tier(), Tier::Standard);
        assert_eq!(TaskMode::Consult.default_tier(), Tier::Heavy);
    }
}
