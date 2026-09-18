//! Model-profile registry (playbook Ch.7 §2.2): per-model capabilities, accepted
//! params, pricing with length tiers, deprecation data. Every routing/pricing/
//! fairness feature depends on this table. Re-verify prices at build time —
//! the appendix flags all pricing as volatile (monthly churn).

use serde::Serialize;

/// Prices in USD per 1M tokens.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct PriceTable {
    pub input: f64,
    pub cache_read: f64,
    /// Anthropic 5-min-TTL cache write multiplier is baked into this price.
    pub cache_write: f64,
    pub output: f64,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct ReasoningSpec {
    /// Whether the model supports a thinking/reasoning budget at all.
    pub supported: bool,
    /// Provider-specific max for thinking budget_tokens; must stay < max_output.
    pub min_budget: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum EditFormat {
    /// Anchored search/replace (the harness `edit` tool's native form).
    SearchReplace,
    /// Unified-diff edits: `edit` accepts `patch`, and an anchor that
    /// differs only in leading whitespace still applies.
    Diff,
    /// Whole-file rewrites: `write` is the intended path, and the contract
    /// segment says so instead of advertising an anchor that will miss.
    WholeFile,
}

impl EditFormat {
    pub const fn as_str(self) -> &'static str {
        match self {
            EditFormat::SearchReplace => "search_replace",
            EditFormat::Diff => "diff",
            EditFormat::WholeFile => "whole_file",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "search_replace" | "search-replace" => Ok(EditFormat::SearchReplace),
            "diff" | "udiff" | "unified_diff" => Ok(EditFormat::Diff),
            "whole_file" | "whole-file" => Ok(EditFormat::WholeFile),
            other => Err(format!(
                "profile: bad edit_format `{other}` — want search_replace|diff|whole_file"
            )),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelProfile {
    pub id: &'static str,
    /// Aliases/prefixes that resolve to this profile (snapshot IDs, short names).
    pub match_prefixes: &'static [&'static str],
    pub context_in: u32,
    pub max_output: u32,
    pub vision: bool,
    pub parallel_calls: bool,
    /// Reasoning/thinking support for the Anthropic family regime.
    pub reasoning: ReasoningSpec,
    /// Compaction trigger as a fraction of `context_in` — the *effective*
    /// window, not the advertised one (playbook Ch.3 §9.2: Claude ~0.83,
    /// Codex ~0.9–0.95, Gemini 0.5; tune down for weak-retrieval models).
    pub compact_at: f32,
    /// Edit dialect the model is trained/known to emit (aider's edit-format
    /// registry, arsenal B2). Drives the `edit` tool's anchor strategy and
    /// the contract line that tells the model which tool to reach for.
    pub edit_format: EditFormat,
    pub price: PriceTable,
}

impl ModelProfile {
    pub fn cost_usd(&self, u: &crate::ir::Usage) -> f64 {
        let per_m = 1_000_000.0;
        (u.fresh_input as f64 * self.price.input
            + u.cache_write as f64 * self.price.cache_write
            + u.cache_read as f64 * self.price.cache_read
            + (u.output + u.reasoning) as f64 * self.price.output)
            / per_m
    }
}

/// Bootstrap table (playbook Ch.7 §1.2 snapshot, Sept 2026 — volatile, re-verify).
/// Ordering matters: first matching prefix wins.
static PROFILES: &[ModelProfile] = &[
    ModelProfile {
        id: "claude-fable-5",
        match_prefixes: &["claude-fable-5", "claude-mythos-5"],
        context_in: 200_000,
        max_output: 64_000,
        vision: true,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 1024,
        },
        compact_at: 0.83,
        edit_format: EditFormat::SearchReplace,
        price: PriceTable {
            input: 10.0,
            cache_read: 1.0,
            cache_write: 12.5,
            output: 50.0,
        },
    },
    ModelProfile {
        id: "claude-opus-4-8",
        match_prefixes: &[
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-opus-4-5",
        ],
        context_in: 200_000,
        max_output: 64_000,
        vision: true,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 1024,
        },
        compact_at: 0.83,
        edit_format: EditFormat::SearchReplace,
        price: PriceTable {
            input: 5.0,
            cache_read: 0.5,
            cache_write: 6.25,
            output: 25.0,
        },
    },
    ModelProfile {
        id: "claude-sonnet-5",
        match_prefixes: &["claude-sonnet-5"],
        context_in: 200_000,
        max_output: 64_000,
        vision: true,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 1024,
        },
        compact_at: 0.83,
        edit_format: EditFormat::SearchReplace,
        price: PriceTable {
            input: 2.0,
            cache_read: 0.2,
            cache_write: 2.5,
            output: 10.0,
        },
    },
    ModelProfile {
        id: "claude-sonnet-4-5",
        match_prefixes: &["claude-sonnet-4-5", "claude-sonnet-4-6"],
        context_in: 200_000,
        max_output: 64_000,
        vision: true,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 1024,
        },
        compact_at: 0.83,
        edit_format: EditFormat::SearchReplace,
        price: PriceTable {
            input: 3.0,
            cache_read: 0.3,
            cache_write: 3.75,
            output: 15.0,
        },
    },
    // --- Fleet fleet (inference.fleet.ai, vLLM-served, Sept 2026) ---
    // Context windows + pricing unpublished → conservative defaults, $0 cost.
    // Re-verify when Fleet publishes limits; ledger cost stays honest
    // (zero, flagged) rather than invented.
    ModelProfile {
        id: "fleet-g53",
        match_prefixes: &["fleet-g53", "fleet/g53"],
        context_in: 131_072,
        max_output: 8_192,
        vision: false,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 0,
        },
        compact_at: 0.70,
        edit_format: EditFormat::Diff,
        price: PriceTable {
            input: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 0.0,
        },
    },
    ModelProfile {
        id: "fleet-g52",
        match_prefixes: &["fleet-g52", "fleet/g52"],
        context_in: 131_072,
        max_output: 8_192,
        vision: false,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 0,
        },
        compact_at: 0.70,
        edit_format: EditFormat::Diff,
        price: PriceTable {
            input: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 0.0,
        },
    },
    ModelProfile {
        id: "fleet-turbo",
        match_prefixes: &["fleet-turbo"],
        context_in: 131_072,
        max_output: 8_192,
        vision: false,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 0,
        },
        compact_at: 0.70,
        edit_format: EditFormat::Diff,
        price: PriceTable {
            input: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 0.0,
        },
    },
    ModelProfile {
        id: "fleet-k3",
        match_prefixes: &["fleet-k3", "fleet/k3"],
        context_in: 131_072,
        max_output: 8_192,
        vision: false,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 0,
        },
        compact_at: 0.70,
        edit_format: EditFormat::Diff,
        price: PriceTable {
            input: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 0.0,
        },
    },
    ModelProfile {
        id: "fleet-q27",
        match_prefixes: &["fleet-q27", "fleet/q27"],
        context_in: 131_072,
        max_output: 8_192,
        vision: false,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 0,
        },
        compact_at: 0.70,
        edit_format: EditFormat::WholeFile,
        price: PriceTable {
            input: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 0.0,
        },
    },
    ModelProfile {
        id: "claude-haiku-4-5",
        match_prefixes: &["claude-haiku-4-5"],
        context_in: 200_000,
        max_output: 64_000,
        vision: true,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 1024,
        },
        compact_at: 0.83,
        edit_format: EditFormat::SearchReplace,
        price: PriceTable {
            input: 1.0,
            cache_read: 0.1,
            cache_write: 1.25,
            output: 5.0,
        },
    },
];

/// Generic fallback so unknown/brand-new models still get sane defaults;
/// priced at a mid-tier estimate, flagged for re-verification.
static FALLBACK: ModelProfile = ModelProfile {
    id: "unknown",
    match_prefixes: &[],
    context_in: 200_000,
    max_output: 32_000,
    vision: false,
    parallel_calls: true,
    reasoning: ReasoningSpec {
        supported: true,
        min_budget: 1024,
    },
    compact_at: 0.80,
    edit_format: EditFormat::SearchReplace,
    price: PriceTable {
        input: 3.0,
        cache_read: 0.3,
        cache_write: 3.75,
        output: 15.0,
    },
};

/// Resolve a model name/alias/snapshot ID to its profile.
pub fn lookup(model: &str) -> &'static ModelProfile {
    PROFILES
        .iter()
        .find(|p| p.match_prefixes.iter().any(|m| model.starts_with(m)))
        .unwrap_or(&FALLBACK)
}

/// True when the model matched a real profile entry — cost figures are
/// then computed from published prices. False means FALLBACK applied and
/// `cost_usd` is a mid-tier estimate, not a measured price (P4.5 honesty:
/// manifests must distinguish the two).
pub fn known(model: &str) -> bool {
    PROFILES
        .iter()
        .any(|p| p.match_prefixes.iter().any(|m| model.starts_with(m)))
}

/// The edit dialect for `model` (aider's edit-format registry): the `edit`
/// tool's anchor strategy and the contract line that names the file-writing
/// tool both read this. Unknown models get the FALLBACK profile's value.
pub fn edit_format(model: &str) -> EditFormat {
    lookup(model).edit_format
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Usage;

    #[test]
    fn lookup_prefix_match() {
        assert_eq!(lookup("claude-sonnet-5-20260101").id, "claude-sonnet-5");
        assert_eq!(lookup("claude-haiku-4-5").id, "claude-haiku-4-5");
        assert_eq!(lookup("some-future-model").id, "unknown");
    }

    #[test]
    fn cost_math() {
        let p = lookup("claude-sonnet-5");
        let u = Usage {
            fresh_input: 1_000_000,
            cache_write: 0,
            cache_read: 10_000_000,
            output: 100_000,
            reasoning: 0,
        };
        // 1M fresh @ $2 + 10M read @ $0.2 + 100K out @ $10 = 2 + 2 + 1 = $5
        assert!((p.cost_usd(&u) - 5.0).abs() < 1e-9);
    }

    #[test]
    fn edit_format_is_per_family_and_parses() {
        // The edit dialect rides the profile: anchors for the Claude
        // family, diffs for the vLLM fleet, whole-file for the small model.
        assert_eq!(edit_format("claude-sonnet-5"), EditFormat::SearchReplace);
        assert_eq!(
            edit_format("claude-opus-4-8-20260301"),
            EditFormat::SearchReplace
        );
        assert_eq!(edit_format("fleet-turbo"), EditFormat::Diff);
        assert_eq!(edit_format("fleet-g53"), EditFormat::Diff);
        assert_eq!(edit_format("fleet-q27"), EditFormat::WholeFile);
        // Unknown model → the FALLBACK profile's dialect, never a panic.
        assert_eq!(edit_format("some-future-model"), EditFormat::SearchReplace);

        assert_eq!(EditFormat::parse("diff").unwrap(), EditFormat::Diff);
        assert_eq!(
            EditFormat::parse("Whole-File").unwrap(),
            EditFormat::WholeFile
        );
        assert!(EditFormat::parse("telepathy")
            .unwrap_err()
            .contains("telepathy"));
        // Every profile declares a dialect (the registry is exhaustive).
        for p in PROFILES {
            assert!(!p.edit_format.as_str().is_empty(), "{} has no format", p.id);
        }
    }
}
