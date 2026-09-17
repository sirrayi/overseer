//! Triage — deterministic classify before anything else touches the event
//! (LangChain ambient-agent pattern, playbook §2.2): every event is
//! ignore / notify / draft-for-review / act. Deterministic v1 — a
//! model-based triage can come later behind the same boundary, but the
//! rules table stays as the fallback and the audit story.

use crate::config::{DaemonConfig, TriageDecision, TriageRule};
use crate::event::TriggerEvent;

/// Triage outcome: the decision plus the scored benefit used downstream
/// by the intervention gate, and the act prompt when decision == Act.
#[derive(Debug, Clone, PartialEq)]
pub struct Triage {
    pub decision: TriageDecision,
    /// 0–100 expected benefit of surfacing/acting on this event.
    pub benefit: u8,
    /// Prompt template for Act (`{payload}` already substituted).
    pub act_prompt: Option<String>,
}

/// Match support: exact class or "*" wildcard; a rule "ci.*" also matches
/// any class under the "ci." prefix.
fn class_matches(rule_class: &str, class: &str) -> bool {
    rule_class == "*"
        || rule_class == class
        || rule_class
            .strip_suffix('*')
            .is_some_and(|prefix| class.starts_with(prefix))
}

/// Classify an event against the rule table; first match wins,
/// `default_decision` covers the unmatched space.
pub fn classify(cfg: &DaemonConfig, ev: &TriggerEvent) -> Triage {
    let (decision, benefit, template) = cfg
        .triage
        .iter()
        .find(|r: &&TriageRule| class_matches(&r.class, &ev.class))
        .map(|r| (r.decision, r.benefit, r.act_prompt.clone()))
        .unwrap_or((cfg.default_decision, 50, None));

    let act_prompt = template.map(|t| t.replace("{payload}", &ev.payload));
    Triage {
        decision,
        benefit,
        act_prompt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DaemonConfig, TriageDecision, TriageRule};
    use crate::event::TriggerEvent;

    fn rule(
        class: &str,
        decision: TriageDecision,
        benefit: u8,
        act_prompt: Option<&str>,
    ) -> TriageRule {
        TriageRule {
            class: class.to_string(),
            decision,
            benefit,
            act_prompt: act_prompt.map(str::to_string),
        }
    }

    fn cfg_with(rules: Vec<TriageRule>, default_decision: TriageDecision) -> DaemonConfig {
        DaemonConfig {
            triage: rules,
            default_decision,
            ..Default::default()
        }
    }

    fn ev(class: &str) -> TriggerEvent {
        TriggerEvent::new("test-source", class, "test-payload")
    }

    #[test]
    fn exact_class_match() {
        let cfg = cfg_with(
            vec![rule("git.dirty", TriageDecision::Notify, 70, None)],
            TriageDecision::Ignore,
        );
        let t = classify(&cfg, &ev("git.dirty"));
        assert_eq!(t.decision, TriageDecision::Notify);
        assert_eq!(t.benefit, 70);
        assert_eq!(t.act_prompt, None);
    }

    #[test]
    fn star_wildcard_matches_any_class() {
        let cfg = cfg_with(
            vec![rule("*", TriageDecision::Ignore, 10, None)],
            TriageDecision::Notify,
        );
        let t = classify(&cfg, &ev("something.entirely-new"));
        assert_eq!(t.decision, TriageDecision::Ignore);
        assert_eq!(t.benefit, 10);
    }

    #[test]
    fn prefix_star_matches_dotted_subclasses() {
        let cfg = cfg_with(
            vec![rule("ci.*", TriageDecision::Notify, 60, None)],
            TriageDecision::Ignore,
        );
        let hit = classify(&cfg, &ev("ci.failed"));
        assert_eq!(hit.decision, TriageDecision::Notify);
        assert_eq!(hit.benefit, 60);
        // Bare "ci" is not under the "ci." prefix; unrelated classes miss too.
        assert_eq!(classify(&cfg, &ev("ci")).decision, TriageDecision::Ignore);
        assert_eq!(
            classify(&cfg, &ev("git.dirty")).decision,
            TriageDecision::Ignore
        );
    }

    #[test]
    fn first_match_wins_ordering() {
        let exact = rule("deploy.done", TriageDecision::Notify, 80, None);
        let catch_all = rule("*", TriageDecision::Ignore, 5, None);
        let cfg = cfg_with(vec![exact.clone(), catch_all.clone()], TriageDecision::Act);
        let t = classify(&cfg, &ev("deploy.done"));
        assert_eq!(t.decision, TriageDecision::Notify);
        assert_eq!(t.benefit, 80);
        // Flipped order: the catch-all shadows the exact rule.
        let flipped = cfg_with(vec![catch_all, exact], TriageDecision::Act);
        let t = classify(&flipped, &ev("deploy.done"));
        assert_eq!(t.decision, TriageDecision::Ignore);
        assert_eq!(t.benefit, 5);
    }

    #[test]
    fn unmatched_class_uses_default_decision_with_benefit_50() {
        let cfg = cfg_with(Vec::new(), TriageDecision::DraftForReview);
        let t = classify(&cfg, &ev("unknown.class"));
        assert_eq!(t.decision, TriageDecision::DraftForReview);
        assert_eq!(t.benefit, 50);
        assert_eq!(t.act_prompt, None);
    }

    #[test]
    fn act_prompt_substitutes_payload() {
        let cfg = cfg_with(
            vec![rule(
                "disk.full",
                TriageDecision::Act,
                90,
                Some("investigate {payload} now"),
            )],
            TriageDecision::Ignore,
        );
        let event = TriggerEvent::new("watch-disk", "disk.full", "disk 99%");
        let t = classify(&cfg, &event);
        assert_eq!(t.decision, TriageDecision::Act);
        assert_eq!(t.act_prompt.as_deref(), Some("investigate disk 99% now"));
    }

    #[test]
    fn missing_template_yields_no_act_prompt() {
        let cfg = cfg_with(
            vec![rule("disk.full", TriageDecision::Act, 90, None)],
            TriageDecision::Ignore,
        );
        let t = classify(&cfg, &ev("disk.full"));
        assert_eq!(t.decision, TriageDecision::Act);
        assert_eq!(t.act_prompt, None);
    }
}
