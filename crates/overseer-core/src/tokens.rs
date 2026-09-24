//! Model-faithful token estimator (B1-9, tiktoken pattern).
//!
//! Overseer budgets on `request_bytes` (bytes != billed tokens). This module
//! adds `count_tokens(text, model)` — a calibrated chars-per-token estimator
//! per model family via `profile::lookup` — so `compact_at` fractions and
//! `max_output` sanity checks are honest about billed tokens.
//!
//! Phase 1: estimator only (no BPE tables, no new deps). Phase 2 (future):
//! vendor rank tables for exact OpenAI counts behind the same function.
//! Call ONLY at budget checkpoints (whitelist): (a) agent.rs effective-window
//! check, (b) enforce_budget observe-only estimate, (c) request max_output
//! sanity. NEVER in hot loops (per-token/per-line). Zero new deps.

/// Estimated tokens for `text` under `model`'s family calibration.
pub fn count_tokens(text: &str, model: &str) -> u64 {
    (text.chars().count() as f64 / chars_per_token(model)).ceil() as u64
}

/// Calibrated chars-per-token per family (conservative: underestimate the
/// divisor → overestimate tokens → compact early, never late).
fn chars_per_token(model: &str) -> f64 {
    let m = model.to_lowercase();
    if m.starts_with("claude") {
        3.5 // Anthropic prose+code mix
    } else if m.starts_with("gpt")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("codex")
    {
        4.0 // OpenAI BPE ~4 chars/token on English/code
    } else if m.starts_with("gemini") || m.starts_with("gemma") {
        3.8
    } else if m.starts_with("kimi")
        || m.starts_with("glm")
        || m.starts_with("qwen")
        || m.starts_with("deepseek")
    {
        3.2 // hosted rows: dense code/stdint, conservative
    } else {
        3.5 // FALLBACK family: match the mid-tier default
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimator_is_monotonic() {
        let a = count_tokens("hi", "claude-sonnet-5");
        let b = count_tokens("hello world, this is longer", "claude-sonnet-5");
        assert!(b > a, "{b} must exceed {a}");
        assert_eq!(count_tokens("", "claude-sonnet-5"), 0);
    }

    #[test]
    fn estimator_is_family_differentiated() {
        let text: String = "x".repeat(400);
        let openai = count_tokens(&text, "gpt-4o");
        let dense = count_tokens(&text, "claude-haiku-4-5");
        // 400/4.0=100 vs 400/3.2=125 — denser family estimates more tokens.
        assert!(dense > openai, "dense {dense} vs openai {openai}");
        assert_eq!(openai, 100);
        assert_eq!(dense, 125);
    }
}
