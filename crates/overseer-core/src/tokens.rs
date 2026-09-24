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

/// Window pressure level (MiMo overflow Window port): how full the context
/// window is, as bands over `used / total`. Fail-closed: `total == 0` is
/// `Critical` (an unknown budget must read as full, never as empty).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pressure {
    Low,
    Guarded,
    High,
    Critical,
}

/// Pressure band for `used` of `total` context: ratio < 0.5 `Low`,
/// < 0.7 `Guarded`, < 0.85 `High`, else `Critical`. `total == 0` yields
/// `Critical`. Integer cross-multiplication on `u128` (no float rounding,
/// no overflow on `u64` inputs).
pub fn window_pressure(used: u64, total: u64) -> Pressure {
    if total == 0 {
        return Pressure::Critical;
    }
    let (used, total) = (used as u128, total as u128);
    if used * 2 < total {
        Pressure::Low
    } else if used * 10 < total * 7 {
        Pressure::Guarded
    } else if used * 20 < total * 17 {
        Pressure::High
    } else {
        Pressure::Critical
    }
}

/// Effective context window for `model` via `profile::lookup` (unknown
/// models get the FALLBACK `context_in`, never 0/panic).
pub fn max_context(model: &str) -> u32 {
    crate::profile::lookup(model).context_in
}

/// Keep the last `budget` chars (the live tail) plus the dropped char
/// count. Char-based, never splits a multi-byte character. `budget == 0`
/// keeps nothing and reports the whole text as dropped.
pub fn budgeted_slice(text: &str, budget: usize) -> (String, usize) {
    let total = text.chars().count();
    if budget == 0 {
        return (String::new(), total);
    }
    if total <= budget {
        return (text.to_string(), 0);
    }
    let dropped = total - budget;
    (text.chars().skip(dropped).collect(), dropped)
}

/// Crop a thinking/reasoning trace to `keep` kept chars: head 1/3 + tail
/// 2/3 joined by a `[…cropped…]` marker. At or under `keep` the text is
/// returned unchanged (no marker); over `keep` the head and tail always
/// sum to exactly `keep` chars. Char-based, never splits mid-character.
pub fn crop_thinking(text: &str, keep: usize) -> String {
    let total = text.chars().count();
    if total <= keep {
        return text.to_string();
    }
    let head_len = keep / 3;
    let tail_len = keep - head_len;
    let head: String = text.chars().take(head_len).collect();
    let tail: String = text.chars().skip(total - tail_len).collect();
    format!("{head}[…cropped…]{tail}")
}

/// Repair line for a lossy history trim: `None` when nothing was dropped,
/// otherwise a line naming the dropped count so the loss is never silent.
pub fn hygiene_note(dropped: usize) -> Option<String> {
    if dropped == 0 {
        None
    } else {
        Some(format!(
            "hygiene: {dropped} chars dropped from history — re-read the tail before continuing"
        ))
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
        let dense = count_tokens(&text, "deepseek-v4.1-flash");
        // 400/4.0=100 vs 400/3.2=125 — denser family estimates more tokens.
        assert!(dense > openai, "dense {dense} vs openai {openai}");
        assert_eq!(openai, 100);
        assert_eq!(dense, 125);
    }

    #[test]
    fn pressure_bands_and_fail_closed_zero_total() {
        assert_eq!(window_pressure(0, 100), Pressure::Low);
        assert_eq!(window_pressure(49, 100), Pressure::Low);
        assert_eq!(window_pressure(50, 100), Pressure::Guarded);
        assert_eq!(window_pressure(69, 100), Pressure::Guarded);
        assert_eq!(window_pressure(70, 100), Pressure::High);
        assert_eq!(window_pressure(84, 100), Pressure::High);
        assert_eq!(window_pressure(85, 100), Pressure::Critical);
        assert_eq!(window_pressure(100, 100), Pressure::Critical);
        // Fail-closed: an unknown/zero budget reads as full.
        assert_eq!(window_pressure(0, 0), Pressure::Critical);
        assert_eq!(window_pressure(5, 0), Pressure::Critical);
    }

    #[test]
    fn max_context_known_and_unknown() {
        assert_eq!(max_context("claude-sonnet-5"), 200_000);
        assert_eq!(max_context("deepseek-v4.1-flash"), 131_072);
        // Unknown models get the FALLBACK window, never 0/panic.
        assert_eq!(max_context("no-such-model-xyz"), 200_000);
    }

    #[test]
    fn slice_keeps_tail_and_reports_dropped() {
        let (kept, dropped) = budgeted_slice("hello world", 5);
        assert_eq!(kept, "world");
        assert_eq!(dropped, 6);
        // At or under budget the text survives whole with zero dropped.
        let (kept, dropped) = budgeted_slice("abc", 3);
        assert_eq!((kept.as_str(), dropped), ("abc", 0));
        let (kept, dropped) = budgeted_slice("abc", 99);
        assert_eq!((kept.as_str(), dropped), ("abc", 0));
        // Zero budget keeps nothing, reports the full char count.
        let (kept, dropped) = budgeted_slice("héllo", 0);
        assert_eq!((kept.as_str(), dropped), ("", 5));
        // Char-based tail: budget 2 of "héllo" keeps "lo", drops 3 chars.
        let (kept, dropped) = budgeted_slice("héllo", 2);
        assert_eq!((kept.as_str(), dropped), ("lo", 3));
    }

    #[test]
    fn crop_thinking_keeps_head_third_and_tail_two_thirds() {
        // Short text passes through with no marker.
        assert_eq!(crop_thinking("abc", 10), "abc");
        let text: String = "x".repeat(30) + &"z".repeat(10) + &"y".repeat(60);
        let out = crop_thinking(&text, 90);
        assert!(out.contains("[…cropped…]"), "{out}");
        let (head, tail) = out.split_once("[…cropped…]").unwrap();
        assert_eq!(head.chars().count(), 30);
        assert_eq!(tail.chars().count(), 60);
        assert!(head.chars().all(|c| c == 'x'));
        assert!(tail.chars().all(|c| c == 'y'));
        // Kept head + tail always sums to exactly `keep` chars.
        let kept: usize = head.chars().count() + tail.chars().count();
        assert_eq!(kept, 90);
    }

    #[test]
    fn hygiene_note_reports_loss_never_silently() {
        assert_eq!(hygiene_note(0), None);
        let note = hygiene_note(42).expect("lossy trim must produce a note");
        assert!(note.contains("42"), "{note}");
    }
}
