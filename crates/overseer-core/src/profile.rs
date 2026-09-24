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
    /// DEFERRED(owner): no tabled row carries this dialect — prompt/edit
    /// coverage is dormant until a WholeFile profile returns.
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

/// Optional wire params per provider family (playbook Ch.7 §2.2). Adapters
/// consult these via [`supports_param`] and strip anything not listed, so a
/// cross-family knob never 400s the request. Names are wire-spelled per API
/// (Anthropic snake_case, Gemini camelCase). No tabled OpenAI/Gemini rows
/// exist yet — gpt-*/gemini-* resolve to FALLBACK, which carries the full
/// OpenAI-compat + Gemini sets; the opencode Go rows carry only the
/// conservative vLLM subset (no reasoning_effort).
const ANTHROPIC_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "top_k",
    "stop_sequences",
    "max_tokens",
    "thinking",
];

/// Full OpenAI-compat set. No tabled row carries it yet (see FALLBACK);
/// kept as the canonical reference — tests assert FALLBACK stays a superset.
#[allow(dead_code)]
const OPENAI_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "max_tokens",
    "reasoning_effort",
];

/// Full Gemini set (generationConfig-spelled). Same note as OPENAI_PARAMS.
#[allow(dead_code)]
const GEMINI_PARAMS: &[&str] = &[
    "temperature",
    "topP",
    "topK",
    "maxOutputTokens",
    "thinkingConfig",
];

/// Conservative vLLM subset for the hosted open-model rows: sampling knobs
/// only. vLLM ignores unknown fields, but reasoning_effort is
/// gateway-specific — stripped fail-closed until the gateway documents it.
const FLEET_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "max_tokens",
];

/// Unknown/brand-new models: permissive OpenAI-compat + Gemini union.
/// Rationale: unknowns are overwhelmingly OpenAI-compatible endpoints or
/// new Gemini snapshots (both ignore unknown fields), while Anthropic-only
/// knobs (top_k/stop_sequences/thinking) stay stripped fail-closed.
const FALLBACK_PARAMS: &[&str] = &[
    "temperature",
    "top_p",
    "frequency_penalty",
    "presence_penalty",
    "max_tokens",
    "reasoning_effort",
    "topP",
    "topK",
    "maxOutputTokens",
    "thinkingConfig",
];

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
    /// Optional wire params this model accepts (wire-spelled per API).
    /// Adapters strip anything not listed before send, so a cross-family
    /// knob never 400s the request. Core keys (model/messages/tools/
    /// max_tokens) are never stripped — they are not params.
    pub accepted_params: &'static [&'static str],
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
        accepted_params: ANTHROPIC_PARAMS,
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
        accepted_params: ANTHROPIC_PARAMS,
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
        accepted_params: ANTHROPIC_PARAMS,
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
        accepted_params: ANTHROPIC_PARAMS,
        price: PriceTable {
            input: 3.0,
            cache_read: 0.3,
            cache_write: 3.75,
            output: 15.0,
        },
    },
    // --- opencode Go fleet (opencode.ai/zen/go, subscription, Sept 2026) ---
    // Context windows unpublished → conservative defaults, $0 cost
    // (subscription-included; the endpoint reports cost "0"). Both rows
    // reason → max_output keeps headroom for thinking traces.
    ModelProfile {
        id: "deepseek-v4.1-flash",
        match_prefixes: &["deepseek-v4.1-flash", "deepseek-v4-flash"],
        context_in: 131_072,
        max_output: 16_384,
        vision: false,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 0,
        },
        compact_at: 0.70,
        edit_format: EditFormat::Diff,
        accepted_params: FLEET_PARAMS,
        price: PriceTable {
            input: 0.0,
            cache_read: 0.0,
            cache_write: 0.0,
            output: 0.0,
        },
    },
    // muse-spark is Responses-API-only on Go (chat/completions 500s) —
    // the opencode provider arm routes muse-* to provider/responses.rs.
    ModelProfile {
        id: "muse-spark-1.3-contributor",
        match_prefixes: &["muse-spark-1.3-contributor", "muse-spark-1.2-contributor"],
        context_in: 131_072,
        max_output: 16_384,
        vision: false,
        parallel_calls: true,
        reasoning: ReasoningSpec {
            supported: true,
            min_budget: 0,
        },
        compact_at: 0.70,
        edit_format: EditFormat::Diff,
        accepted_params: FLEET_PARAMS,
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
        accepted_params: ANTHROPIC_PARAMS,
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
    accepted_params: FALLBACK_PARAMS,
    price: PriceTable {
        input: 3.0,
        cache_read: 0.3,
        cache_write: 3.75,
        output: 15.0,
    },
};

/// True when `model` accepts the optional wire param `param` (wire-spelled:
/// Anthropic snake_case, Gemini camelCase). Unknown models consult FALLBACK
/// (the permissive OpenAI-compat + Gemini union), never panic.
///
/// Core keys (`model`, `messages`, `tools`, `max_tokens`/`maxOutputTokens`)
/// are NOT params: adapters build them unconditionally and must never strip
/// them. Passing one here returns false so no caller can accidentally gate a
/// required key. Comparison is exact-case: `top_p` (OpenAI/Anthropic) and
/// `topP` (Gemini) are different wire names on purpose.
pub fn supports_param(model: &str, param: &str) -> bool {
    if matches!(
        param,
        "model" | "messages" | "tools" | "max_tokens" | "maxOutputTokens"
    ) {
        return false;
    }
    lookup(model).accepted_params.contains(&param)
}
/// Remove optional keys `body` carries that `model` does not accept. `keys`
/// names the optional top-level (or generationConfig-level — each caller
/// checks its own object) candidates the adapter supports emitting; only
/// listed keys are touched, and only when [`supports_param`] says no.
/// Deterministic: `keys` order decides removal order; untouched otherwise
/// (byte-stable when every key is supported — see adapter tests). Core keys
/// (`model`, `messages`, `tools`, `max_tokens`, `maxOutputTokens`) are never
/// removed even if named in `keys` — they are required, not params.
pub fn strip_optional_params(body: &mut serde_json::Value, model: &str, keys: &[&str]) {
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    for key in keys {
        // Core keys are required, not params — never remove even if named.
        if matches!(
            *key,
            "model" | "messages" | "tools" | "max_tokens" | "maxOutputTokens"
        ) {
            continue;
        }
        if !supports_param(model, key) {
            obj.remove(*key);
        }
    }
}

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
        // family, diffs for the hosted open-model rows.
        assert_eq!(edit_format("claude-sonnet-5"), EditFormat::SearchReplace);
        assert_eq!(
            edit_format("claude-opus-4-8-20260301"),
            EditFormat::SearchReplace
        );
        assert_eq!(edit_format("deepseek-v4.1-flash"), EditFormat::Diff);
        assert_eq!(edit_format("muse-spark-1.3-contributor"), EditFormat::Diff);
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

    #[test]
    fn supports_param_per_family_and_fallback() {
        // Anthropic family: own knobs accepted, cross-family rejected.
        assert!(supports_param("claude-sonnet-5", "temperature"));
        assert!(supports_param("claude-sonnet-5", "top_k"));
        assert!(supports_param("claude-sonnet-5", "stop_sequences"));
        assert!(supports_param("claude-sonnet-5", "thinking"));
        assert!(!supports_param("claude-sonnet-5", "reasoning_effort"));
        assert!(!supports_param("claude-sonnet-5", "frequency_penalty"));
        // Wire spelling is exact-case: top_p ≠ topP.
        assert!(!supports_param("claude-sonnet-5", "topP"));
        // Hosted open-model rows: conservative vLLM subset, no
        // reasoning_effort.
        assert!(supports_param("deepseek-v4.1-flash", "temperature"));
        assert!(supports_param("deepseek-v4.1-flash", "frequency_penalty"));
        assert!(!supports_param("deepseek-v4.1-flash", "reasoning_effort"));
        assert!(!supports_param("deepseek-v4.1-flash", "thinking"));
        // Unknown models → FALLBACK union (OpenAI-compat + Gemini), so new
        // gpt/gemini snapshots send their native knobs through.
        assert!(!known("some-future-model"));
        assert_eq!(lookup("some-future-model").id, "unknown");
        assert!(supports_param("some-future-model", "reasoning_effort"));
        assert!(supports_param("some-future-model", "thinkingConfig"));
        assert!(supports_param("some-future-model", "topP"));
        assert!(!supports_param("some-future-model", "thinking"));
        // Core keys are never params — supports_param refuses to gate them
        // so no strip call can remove a required key.
        for core in [
            "model",
            "messages",
            "tools",
            "max_tokens",
            "maxOutputTokens",
        ] {
            assert!(!supports_param("claude-sonnet-5", core), "{core} gated");
            assert!(!supports_param("some-future-model", core), "{core} gated");
        }
        // FALLBACK stays a superset of the canonical family sets: if a row
        // is ever added for gpt-*/gemini-*, its knobs already pass.
        for p in OPENAI_PARAMS {
            assert!(FALLBACK_PARAMS.contains(p), "fallback lacks {p}");
        }
        for p in GEMINI_PARAMS {
            assert!(FALLBACK_PARAMS.contains(p), "fallback lacks {p}");
        }
        // Every tabled accepted_params list is non-empty (no dead row).
        for p in PROFILES {
            assert!(!p.accepted_params.is_empty(), "{} has no params", p.id);
        }
        assert!(!FALLBACK.accepted_params.is_empty());
    }
}
