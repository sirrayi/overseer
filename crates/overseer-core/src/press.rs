//! Context-compression patterns ported from the press batch (arsenal B2).
//!
//! Two ports, both pure and dependency-free. The engine links no model, so
//! what is ported is the *shape* of each compressor plus the budget
//! discipline that keeps a lossy rewrite auditable:
//!
//! - **llmlingua compression slot** — llmlingua's `PromptCompressor` takes a
//!   long text and a target rate and asks a model to rewrite it smaller. The
//!   port is the slot (`Compressor`), the honest default for an engine with no
//!   weights linked (`IdentityCompressor`), and the accounting (`Pressed`)
//!   that says how much text there was, how much the budget allowed, and
//!   whether the budget — not the compressor — decided the final length.
//! - **kvpress `ContextPress`** — kv-press evicts cache entries under memory
//!   pressure while the attention sink (the head) and the recent tail
//!   survive. The text-layer analogue (`SinkRecentPress`) evicts middle
//!   *segments* under the same priority, and reports exactly which indices
//!   were dropped so the loss is never hidden.
//!
//! Budgets count chars, never bytes: a multi-byte character costs one char
//! and a cap never lands mid-character.
//!
//! `// DEFERRED(owner): actually rewriting text with a model (llmlingua's
//! `PromptCompressor` needs weights; `IdentityCompressor` returns its input
//! unchanged), a tokenizer (budgets here are chars, not tokens), and any
//! KV-cache access (kv-press is ported as an eviction policy over text
//! segments, not as a cache operator) — this batch lands the slots, the
//! budgets, and the reports only.`

use serde_json::Value;

// ── llmlingua compression slot ───────────────────────────────────────────

/// The compression slot (llmlingua's `PromptCompressor` shape): rewrite
/// `text` toward `rate` — the fraction of the original to keep.
///
/// Invariants: `press` validates `rate` (finite, in `(0.0, 1.0]`) and
/// hard-caps whatever comes back, so an implementation never has to enforce
/// the budget itself. Implementations MUST be deterministic for the same
/// `(text, rate)` — an eval that presses a prompt must measure the model
/// under test, not compressor jitter — and MUST return `Err` (reason
/// included) when they cannot compress rather than pretending to have done
/// so.
pub trait Compressor {
    /// Rewrite `text` toward `rate` (fraction of the original size to keep).
    /// `rate` has already passed `validate`, so the value is always finite
    /// and in `(0.0, 1.0]` here.
    fn compress(&self, text: &str, rate: f64) -> Result<String, String>;
}

/// The shipped default: no model is linked, so nothing is rewritten.
///
/// It is deliberately an identity and not a fake compressor — a caller that
/// wants real compression supplies its own `Compressor`, and a caller that
/// only wants the budget enforced still gets the hard cap and the accounting
/// (the output is byte-for-byte the input, so nothing is silently lost).
#[derive(Debug, Clone, Copy, Default)]
pub struct IdentityCompressor;

impl Compressor for IdentityCompressor {
    fn compress(&self, text: &str, _rate: f64) -> Result<String, String> {
        Ok(text.to_string())
    }
}

/// The hard ceiling one press must respect: at most `max_chars` characters of
/// output, always — a result over it is never returned un-capped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PressBudget {
    /// Ceiling in characters, never bytes. Must be ≥ 1; `press` rejects 0
    /// (a zero budget can hold nothing and is a caller bug, not a policy).
    pub max_chars: usize,
}

/// What a press produced, with the accounting that makes it auditable.
///
/// `original_chars` is the input's char count, `hard_capped` says the budget
/// (not the compressor) decided the final length, and `note` says the same
/// thing in words. Logging `(original_chars, budget_chars, kept)` is enough
/// to see exactly how much context was spent.
#[derive(Debug, Clone, PartialEq)]
pub struct Pressed {
    /// The text the caller may use: never longer than `budget_chars` chars.
    pub text: String,
    /// Char count of the *original* input (chars, not bytes).
    pub original_chars: usize,
    /// The ceiling the press ran under.
    pub budget_chars: usize,
    /// The requested keep-rate, echoed whether or not the compressor was
    /// consulted (an identity short-circuit still reports the request).
    pub rate: f64,
    /// True when `text` was cut at `budget_chars` after the compressor ran.
    pub hard_capped: bool,
    /// `Some` exactly when the compressor's output had to be hard-capped,
    /// naming the cap; a capped result is never silent.
    pub note: Option<String>,
}

/// The one gate on a press: `max_chars` must be ≥ 1 and `rate` must be finite
/// and in `(0.0, 1.0]`. Exported so a caller can pre-check a budget without a
/// compressible text; `press` runs it before anything else, including the
/// identity short-circuit.
pub fn validate(max_chars: usize, rate: f64) -> Result<(), String> {
    if max_chars == 0 {
        return Err(
            "press: max_chars is 0 — the budget must be at least 1 char (a zero budget can \
             hold nothing)"
                .to_string(),
        );
    }
    if !rate.is_finite() || rate <= 0.0 || rate > 1.0 {
        return Err(format!(
            "press: `rate` must be finite and in (0.0, 1.0], got {rate} (1.0 keeps everything, \
             smaller asks for more compression)"
        ));
    }
    Ok(())
}

/// Press `text` into `budget` at `rate`, using `c` only when the text does not
/// already fit.
///
/// Invariants (all asserted by the tests):
/// - validation runs first: a zero budget or an out-of-band rate errors and
///   the message names the band;
/// - at or under budget the result is the input unchanged, with
///   `hard_capped: false`, `note: None`, and the compressor *not called*
///   (an under-budget press costs nothing);
/// - over budget the compressor is called once with the validated `rate`, and
///   its error propagates verbatim (a failed compression must not read as a
///   successful press);
/// - output is never longer than `budget.max_chars` chars: a compressor result
///   still over budget is hard-capped at a char boundary (multi-byte safe)
///   with `hard_capped: true` and a note naming the cap;
/// - `original_chars` counts the original text's chars, not its bytes.
pub fn press(
    text: &str,
    budget: PressBudget,
    rate: f64,
    c: &impl Compressor,
) -> Result<Pressed, String> {
    validate(budget.max_chars, rate)?;
    let original_chars = nchars(text);
    if original_chars <= budget.max_chars {
        return Ok(Pressed {
            text: text.to_string(),
            original_chars,
            budget_chars: budget.max_chars,
            rate,
            hard_capped: false,
            note: None,
        });
    }
    let compressed = c.compress(text, rate)?;
    let compressed_chars = nchars(&compressed);
    if compressed_chars <= budget.max_chars {
        return Ok(Pressed {
            text: compressed,
            original_chars,
            budget_chars: budget.max_chars,
            rate,
            hard_capped: false,
            note: None,
        });
    }
    Ok(Pressed {
        text: head_chars(&compressed, budget.max_chars),
        original_chars,
        budget_chars: budget.max_chars,
        rate,
        hard_capped: true,
        note: Some(format!(
            "hard-capped: compressor returned {compressed_chars} chars for a budget of {} chars \
             — kept the first {} chars at a char boundary",
            budget.max_chars, budget.max_chars
        )),
    })
}

// ── kvpress ContextPress ─────────────────────────────────────────────────

/// The eviction policy (kv-press `ContextPress` shape): given ordered
/// segments and a char budget, decide which indices survive.
///
/// Invariants: the report's `kept_idx`/`dropped_idx` are a *partition* of
/// `0..segments.len()` — every index in exactly one list, both ascending, so a
/// caller rebuilds the surviving context in original order — and `note` is
/// `Some` whenever anything was dropped (an eviction is reported, never
/// silent).
pub trait ContextPress {
    /// Choose the surviving segments. `budget_chars == 0` is legal and yields
    /// a valid (empty-kept) partition with a note.
    fn press(&self, segments: &[String], budget_chars: usize) -> PressReport;
}

/// The eviction result.
///
/// `kept_idx` is ascending, so the surviving context is
/// `kept_idx.iter().map(|&i| &segments[i])` with no re-sorting. The char
/// totals say what the budget actually bought, and `note` is `Some` whenever
/// the report is lossy; an over-budget pinned floor is visible in
/// `kept_chars > budget_chars` even when nothing could be dropped.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PressReport {
    /// Surviving segment indices, ascending.
    pub kept_idx: Vec<usize>,
    /// Evicted segment indices, ascending.
    pub dropped_idx: Vec<usize>,
    /// Char count of the kept segments.
    pub kept_chars: usize,
    /// Char count of the dropped segments.
    pub dropped_chars: usize,
    /// `Some` when anything was dropped, explaining what and why.
    pub note: Option<String>,
}

/// Attention-sink + recency-tail policy: the first `sink` and last `recent`
/// segments are pinned (the head is what a model anchors on, the tail is the
/// live turn) and only the middle is evictable.
///
/// Invariants: `sink`/`recent` larger than the segment list clamp to it, and
/// when they overlap every segment is pinned (the head is never evicted).
/// The sink is kept *in full* even when it alone exceeds the budget — that is
/// what "attention sink" means, and the shortfall is reported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SinkRecentPress {
    /// How many leading segments are pinned.
    pub sink: usize,
    /// How many trailing segments are pinned.
    pub recent: usize,
}

impl ContextPress for SinkRecentPress {
    /// Priority order, deterministic: the sink (always, in full), then the
    /// recency tail newest-first while it fits, then the middle nearest to the
    /// tail first — so a tight budget buys the most recent context. Kept
    /// indices are returned ascending regardless of that fill order.
    fn press(&self, segments: &[String], budget_chars: usize) -> PressReport {
        let n = segments.len();
        let chars: Vec<usize> = segments.iter().map(|s| nchars(s)).collect();
        let sink_n = self.sink.min(n);
        let tail_start = n.saturating_sub(self.recent.min(n));
        let pinned_chars: usize = (0..n)
            .filter(|&i| i < sink_n || i >= tail_start)
            .map(|i| chars[i])
            .sum();

        let mut keep = vec![false; n];
        // The sink is pinned unconditionally: it survives a budget that cannot
        // hold it (kv-press's attention sink is never evicted).
        let mut used = 0usize;
        for i in 0..sink_n {
            keep[i] = true;
            used += chars[i];
        }
        // The recency tail next, newest first, against what is left of the
        // budget after the sink.
        for i in (tail_start..n).rev() {
            if keep[i] {
                continue; // already pinned by the sink (sink + recent overlap)
            }
            if used + chars[i] <= budget_chars {
                keep[i] = true;
                used += chars[i];
            }
        }
        // Then the middle, nearest to the tail first: it is the evictable
        // region, and the newest half of it is the useful half.
        for i in (sink_n..tail_start).rev() {
            if used + chars[i] <= budget_chars {
                keep[i] = true;
                used += chars[i];
            }
        }

        let kept_idx: Vec<usize> = (0..n).filter(|&i| keep[i]).collect();
        let dropped_idx: Vec<usize> = (0..n).filter(|&i| !keep[i]).collect();
        let kept_chars: usize = kept_idx.iter().map(|&i| chars[i]).sum();
        let dropped_chars: usize = dropped_idx.iter().map(|&i| chars[i]).sum();
        let note = if dropped_idx.is_empty() {
            None
        } else if pinned_chars > budget_chars {
            Some(format!(
                "budget {budget_chars} chars is under the pinned floor ({pinned_chars} chars): \
                 kept the {sink_n} sink segment(s) in full, dropped {} segment(s) ({} chars) — \
                 {} char(s) kept, over budget by {} char(s)",
                dropped_idx.len(),
                dropped_chars,
                kept_chars,
                kept_chars.saturating_sub(budget_chars)
            ))
        } else {
            Some(format!(
                "budget {budget_chars} chars: evicted {} middle segment(s) ({} chars), kept {} \
                 char(s) in original order",
                dropped_idx.len(),
                dropped_chars,
                kept_chars
            ))
        };
        PressReport {
            kept_idx,
            dropped_idx,
            kept_chars,
            dropped_chars,
            note,
        }
    }
}

// ── microcompact whitelist ─────────────────────────────────────────────────

/// Keys a microcompact may keep (MiMo manifest port): the goal, the
/// outstanding work, what already broke, which files matter, and what was
/// decided. Everything else is dropped so a compacted manifest stays small
/// and deterministic.
pub const MICROCOMPACT_KEEP: &[&str] = &["goal", "pending", "errors", "files", "decisions"];

/// Keep only [`MICROCOMPACT_KEEP`] keys of a JSON object. Non-objects pass
/// through unchanged (there is nothing to trim). Deterministic: `serde_json`
/// maps are `BTreeMap`s, so the surviving keys stay sorted with no extra
/// work.
pub fn microcompact_filter(value: &Value) -> Value {
    let Value::Object(obj) = value else {
        return value.clone();
    };
    obj.iter()
        .filter(|(k, _)| MICROCOMPACT_KEEP.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

/// True when [`microcompact_filter`] would drop nothing: non-objects are
/// trivially safe, and objects are safe exactly when every key is
/// whitelisted.
pub fn is_microcompact_safe(value: &Value) -> bool {
    match value {
        Value::Object(obj) => obj.keys().all(|k| MICROCOMPACT_KEEP.contains(&k.as_str())),
        _ => true,
    }
}

/// Char count of `s` — a budget is spent in chars so a multi-byte character
/// costs one.
fn nchars(s: &str) -> usize {
    s.chars().count()
}

/// The first `cap` chars of `s`. Taking chars can only land on a char
/// boundary, so the result is valid UTF-8 and never splits a multi-byte
/// character (the reason a byte slice would be wrong here).
fn head_chars(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A compressor that counts its calls and returns a fixed reply, so "the
    /// compressor was not called" is asserted rather than inferred.
    struct Spy {
        calls: Cell<usize>,
        reply: Result<String, String>,
    }

    impl Spy {
        fn ok(text: &str) -> Self {
            Spy {
                calls: Cell::new(0),
                reply: Ok(text.to_string()),
            }
        }

        fn failing(msg: &str) -> Self {
            Spy {
                calls: Cell::new(0),
                reply: Err(msg.to_string()),
            }
        }
    }

    impl Compressor for Spy {
        fn compress(&self, _text: &str, _rate: f64) -> Result<String, String> {
            self.calls.set(self.calls.get() + 1);
            self.reply.clone()
        }
    }

    fn segs(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    /// The partition invariant both reports promise: ascending, disjoint,
    /// covering every index, with char totals that match the lists — and a
    /// note whenever anything was dropped.
    fn assert_partition(r: &PressReport, segments: &[String]) {
        assert!(
            r.kept_idx.windows(2).all(|w| w[0] < w[1]),
            "kept_idx not ascending: {r:?}"
        );
        assert!(
            r.dropped_idx.windows(2).all(|w| w[0] < w[1]),
            "dropped_idx not ascending: {r:?}"
        );
        let mut all: Vec<usize> = r
            .kept_idx
            .iter()
            .chain(r.dropped_idx.iter())
            .copied()
            .collect();
        all.sort_unstable();
        assert_eq!(
            all,
            (0..segments.len()).collect::<Vec<_>>(),
            "kept/dropped is not a partition: {r:?}"
        );
        let sum = |idx: &[usize]| {
            idx.iter()
                .map(|&i| segments[i].chars().count())
                .sum::<usize>()
        };
        assert_eq!(r.kept_chars, sum(&r.kept_idx), "kept_chars wrong: {r:?}");
        assert_eq!(
            r.dropped_chars,
            sum(&r.dropped_idx),
            "dropped_chars wrong: {r:?}"
        );
        if !r.dropped_idx.is_empty() {
            assert!(r.note.is_some(), "lossy report without a note: {r:?}");
        }
    }

    #[test]
    fn press_under_budget_is_identity_and_never_calls_the_compressor() {
        let spy = Spy::ok("REWRITTEN — must not appear");
        let out = press("short text", PressBudget { max_chars: 100 }, 0.5, &spy).unwrap();
        assert_eq!(out.text, "short text");
        assert_eq!(out.original_chars, 10);
        assert_eq!(out.budget_chars, 100);
        assert_eq!(out.rate, 0.5);
        assert!(!out.hard_capped);
        assert_eq!(out.note, None);
        assert_eq!(
            spy.calls.get(),
            0,
            "an under-budget press must cost nothing"
        );

        // Exactly at the budget counts as fitting (the boundary is inclusive).
        let spy = Spy::ok("REWRITTEN");
        let out = press("0123456789", PressBudget { max_chars: 10 }, 0.5, &spy).unwrap();
        assert_eq!(out.text, "0123456789");
        assert_eq!(spy.calls.get(), 0);
        assert!(!out.hard_capped);

        // An empty text fits any valid budget.
        let spy = Spy::ok("REWRITTEN");
        let out = press("", PressBudget { max_chars: 1 }, 1.0, &spy).unwrap();
        assert_eq!(out.text, "");
        assert_eq!(out.original_chars, 0);
        assert_eq!(spy.calls.get(), 0);
    }

    #[test]
    fn press_over_budget_calls_the_compressor_once_and_reports_its_text() {
        let spy = Spy::ok("tiny");
        let long = "x".repeat(50);
        let out = press(&long, PressBudget { max_chars: 20 }, 0.25, &spy).unwrap();
        assert_eq!(spy.calls.get(), 1);
        assert_eq!(out.text, "tiny");
        assert_eq!(out.original_chars, 50);
        assert_eq!(out.budget_chars, 20);
        assert_eq!(out.rate, 0.25);
        assert!(!out.hard_capped, "the compressor already fit the budget");
        assert_eq!(out.note, None);
    }

    #[test]
    fn press_rejects_bad_rate_and_zero_budget_naming_the_valid_band() {
        for rate in [0.0, -0.5, 1.5, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let spy = Spy::ok("unused");
            let e = press("some text", PressBudget { max_chars: 4 }, rate, &spy).unwrap_err();
            assert!(
                e.contains("rate") && e.contains("(0.0, 1.0]"),
                "rate {rate} gave {e:?}"
            );
            assert_eq!(
                spy.calls.get(),
                0,
                "invalid input must not reach the compressor"
            );
        }
        let spy = Spy::ok("unused");
        let e = press("some text", PressBudget { max_chars: 0 }, 0.5, &spy).unwrap_err();
        assert!(e.contains("max_chars") && e.contains("1"), "{e:?}");
        assert_eq!(spy.calls.get(), 0);
        // The gate runs before the identity short-circuit: a zero budget is a
        // caller bug even for an empty text.
        assert!(press("", PressBudget { max_chars: 0 }, 0.5, &IdentityCompressor).is_err());
        // Rates inside the band are accepted, endpoints included.
        assert!(press(
            "abc",
            PressBudget { max_chars: 3 },
            1.0,
            &IdentityCompressor
        )
        .is_ok());
        assert!(press(
            "abc",
            PressBudget { max_chars: 3 },
            f64::MIN_POSITIVE,
            &IdentityCompressor
        )
        .is_ok());
    }

    #[test]
    fn press_propagates_a_compressor_error_verbatim() {
        let spy = Spy::failing("compressor: no weights linked for rate 0.5");
        let e = press(&"y".repeat(30), PressBudget { max_chars: 5 }, 0.5, &spy).unwrap_err();
        assert_eq!(e, "compressor: no weights linked for rate 0.5");
        assert_eq!(spy.calls.get(), 1);
    }

    #[test]
    fn press_hard_cap_stays_on_char_boundaries_and_says_so() {
        let cjk = "压缩上下文";
        let wide = cjk.repeat(4); // 20 chars, 60 bytes
        let spy = Spy::ok(&wide);
        let out = press(&"z".repeat(40), PressBudget { max_chars: 7 }, 0.1, &spy).unwrap();
        assert!(out.hard_capped);
        assert_eq!(out.text.chars().count(), 7);
        assert_eq!(out.text, wide.chars().take(7).collect::<String>());
        // 7 chars of 3-byte CJK characters = 21 bytes: a byte cap would have
        // produced 7 bytes and split a character.
        assert_eq!(out.text.len(), 21);
        let note = out.note.expect("a hard cap is never silent");
        assert!(note.contains("hard-capped") && note.contains("7"), "{note}");

        // Same property with 2-byte characters.
        let accented = "é".repeat(40);
        let spy = Spy::ok(&accented);
        let out = press(&"z".repeat(30), PressBudget { max_chars: 5 }, 0.5, &spy).unwrap();
        assert_eq!(out.text, "ééééé");
        assert_eq!(out.text.len(), 10);
        assert!(out.hard_capped);
    }

    #[test]
    fn press_counts_original_chars_not_bytes() {
        let text = "é".repeat(30); // 30 chars, 60 bytes
        let out = press(
            &text,
            PressBudget { max_chars: 100 },
            0.5,
            &IdentityCompressor,
        )
        .unwrap();
        assert_eq!(out.original_chars, 30);
        assert_eq!(out.text.chars().count(), 30);
        assert!(!out.hard_capped);

        let out = press(
            &text,
            PressBudget { max_chars: 10 },
            0.5,
            &IdentityCompressor,
        )
        .unwrap();
        assert_eq!(out.original_chars, 30, "bytes (60) must not be reported");
        assert_eq!(out.text, "é".repeat(10));
        assert_eq!(out.text.chars().count(), 10);
        assert!(out.hard_capped);
    }

    #[test]
    fn sink_and_recent_survive_and_the_middle_is_evicted_nearest_to_tail_first() {
        let segments = segs(&["HEAD-a", "m1", "m2", "m3", "TAIL"]);
        let presser = SinkRecentPress { sink: 1, recent: 1 };
        // Pinned 6 + 4 = 10 chars of a 12-char budget: the last 2 chars go to
        // the middle segment nearest the tail (m3), m2 and m1 are evicted.
        let r = presser.press(&segments, 12);
        assert_eq!(r.kept_idx, vec![0, 3, 4]);
        assert_eq!(r.dropped_idx, vec![1, 2]);
        assert_eq!(r.kept_chars, 12);
        assert_eq!(r.dropped_chars, 4);
        let note = r.note.clone().expect("evictions are reported");
        assert!(
            note.contains("evicted") && note.contains("2 middle segment"),
            "{note}"
        );
        assert_partition(&r, &segments);

        // A generous budget keeps everything, with no note.
        let r = presser.press(&segments, 1_000);
        assert_eq!(r.kept_idx, vec![0, 1, 2, 3, 4]);
        assert!(r.dropped_idx.is_empty());
        assert_eq!(r.kept_chars, 16);
        assert_eq!(r.note, None);
        assert_partition(&r, &segments);
    }

    #[test]
    fn a_budget_under_the_pinned_floor_keeps_the_sink_in_full_and_explains_the_shortfall() {
        let segments = segs(&["HEAD-a", "m1", "m2", "m3", "TAIL"]);
        let presser = SinkRecentPress { sink: 1, recent: 1 };
        let r = presser.press(&segments, 5); // 10 pinned chars > 5
        assert_eq!(r.kept_idx, vec![0], "the sink survives even over budget");
        assert_eq!(r.dropped_idx, vec![1, 2, 3, 4]);
        assert_eq!(r.kept_chars, 6);
        assert_eq!(r.dropped_chars, 10);
        let note = r
            .note
            .clone()
            .expect("an over-budget floor is never silent");
        assert!(
            note.contains("pinned floor") && note.contains("over budget"),
            "{note}"
        );
        assert_partition(&r, &segments);

        // Under the floor, the recency tail outranks the older pinned head:
        // sink "old" (3) fits, then newest-first "new" (3) fills the budget,
        // "mid" is dropped though it is also pinned.
        let segments = segs(&["old", "mid", "new"]);
        let r = SinkRecentPress { sink: 1, recent: 2 }.press(&segments, 6);
        assert_eq!(r.kept_idx, vec![0, 2]);
        assert_eq!(r.dropped_idx, vec![1]);
        assert_eq!(r.kept_chars, 6);
        assert_eq!(r.dropped_chars, 3);
        assert!(r.note.is_some());
        assert_partition(&r, &segments);
    }

    #[test]
    fn every_report_is_a_partition_for_any_policy_and_budget() {
        let segments = segs(&["aa", "bb", "cc", "dd", "ee"]);
        let n = segments.len();
        for (sink, recent) in [(0, 0), (1, 1), (2, 3), (5, 5), (3, 2), (0, 5), (9, 0)] {
            for budget in [0usize, 1, 3, 7, 100] {
                let r = SinkRecentPress { sink, recent }.press(&segments, budget);
                assert_partition(&r, &segments);
                let sink_n = sink.min(n);
                let tail_start = n.saturating_sub(recent.min(n));
                let pinned: usize = (0..n)
                    .filter(|&i| i < sink_n || i >= tail_start)
                    .map(|i| segments[i].chars().count())
                    .sum();
                assert!(
                    r.kept_chars <= budget || pinned > budget,
                    "overshot a satisfiable budget: sink={sink} recent={recent} budget={budget} \
                     {r:?}"
                );
            }
        }
    }

    #[test]
    fn zero_budget_still_yields_a_partition_and_drops_everything_evictable() {
        let segments = segs(&["aa", "bb", "cc"]);
        let r = SinkRecentPress { sink: 0, recent: 0 }.press(&segments, 0);
        assert!(r.kept_idx.is_empty());
        assert_eq!(r.dropped_idx, vec![0, 1, 2]);
        assert_eq!(r.kept_chars, 0);
        assert_eq!(r.dropped_chars, 6);
        assert!(r.note.is_some(), "a zero budget must say what it dropped");
        assert_partition(&r, &segments);

        // Nothing pinned and nothing to pin: still a valid (empty) partition.
        let r = SinkRecentPress { sink: 1, recent: 1 }.press(&[], 0);
        assert_eq!(r, PressReport::default());
        assert_eq!(r.note, None);
    }

    #[test]
    fn overlapping_sink_and_recent_pin_everything_without_double_counting() {
        let segments = segs(&["a", "b"]);
        let r = SinkRecentPress { sink: 9, recent: 9 }.press(&segments, 1);
        assert_eq!(r.kept_idx, vec![0, 1], "every segment is pinned");
        assert!(r.dropped_idx.is_empty());
        assert_eq!(r.kept_chars, 2, "the overlap is not counted twice");
        assert_eq!(r.dropped_chars, 0);
        // Nothing was dropped, so there is nothing to report — the over-budget
        // floor is still visible in kept_chars (2) > budget (1).
        assert_eq!(r.note, None);
        assert_partition(&r, &segments);
    }

    #[test]
    fn pressing_equal_inputs_is_deterministic() {
        for (sink, recent, budget) in [(1, 1, 6), (2, 1, 4), (0, 0, 0), (3, 3, 100)] {
            let p = SinkRecentPress { sink, recent };
            let segments = segs(&["one", "two", "three", "four"]);
            let first = p.press(&segments, budget);
            assert_eq!(first, p.press(&segments, budget));
            let equal_copy = segs(&["one", "two", "three", "four"]);
            assert_eq!(first, p.press(&equal_copy, budget));
        }
        // The compressor path is deterministic too: identity in, identity out.
        let out = press(
            "a long enough text",
            PressBudget { max_chars: 4 },
            0.5,
            &IdentityCompressor,
        )
        .unwrap();
        assert_eq!(out.text, "a lo");
        assert!(
            out.hard_capped,
            "an identity compressor still respects the budget"
        );
        assert!(out.note.is_some());
    }

    #[test]
    fn microcompact_keeps_only_whitelisted_keys() {
        use serde_json::json;
        let value = json!({
            "goal": "ship",
            "pending": ["a"],
            "errors": ["e"],
            "files": ["f.rs"],
            "decisions": ["d"],
            "transcript": "drop me",
            "scratch": 7,
        });
        let filtered = microcompact_filter(&value);
        assert_eq!(
            filtered,
            json!({
                "goal": "ship",
                "pending": ["a"],
                "errors": ["e"],
                "files": ["f.rs"],
                "decisions": ["d"],
            })
        );
        assert!(
            !is_microcompact_safe(&value),
            "extra keys must read as unsafe"
        );
        assert!(is_microcompact_safe(&filtered));
        assert!(is_microcompact_safe(&json!({"goal": "x"})));
        assert!(
            is_microcompact_safe(&json!({})),
            "empty object drops nothing"
        );
    }

    #[test]
    fn microcompact_passes_non_objects_through() {
        use serde_json::json;
        for value in [
            json!(7),
            json!("text"),
            json!([1, 2]),
            json!(null),
            json!(true),
        ] {
            assert_eq!(microcompact_filter(&value), value, "passthrough: {value}");
            assert!(is_microcompact_safe(&value), "passthrough is safe: {value}");
        }
    }
}
