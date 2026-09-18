//! Fuzzy matching for lookup-miss hints (fzf pattern, arsenal B2).
//!
//! When a `read`/`skill` lookup misses, the useful reply is not "not found"
//! but "did you mean …". That needs the ranking fzf uses: subsequence match
//! with a boundary-aware score, so `src/fuzzy.rs` beats `tests/fuzzy_old.rs`
//! for the query `fzrs`.
//!
//! Scoring uses fzf's published constants (bonus for a match at a word
//! boundary, camelCase/digit transition, or right after a separator; penalty
//! for skipped characters, larger at the start of a gap; extra bonus for
//! consecutive matches; the first query character's bonus is doubled).
//! Window selection is fzf V1's two passes: scan forward to the earliest
//! index where the query completes (the existence test), then walk backwards
//! from there to the earliest start (the tightest window ending at that
//! index). The V2 optimal-window pass (a forward DP that also tries later
//! ends to pick up better bonuses) is not ported; the score table is the
//! same, so a future DP changes *which* span wins, not what a span is worth.
//! `// DEFERRED(owner): FuzzyMatchV2's optimal-window DP and the tiered
//! prefix/exact fast paths — the greedy window plus the V2 constants cover
//! the miss-hint use case; port the DP if hints ever rank visibly wrong.`

/// fzf's score constants (V2 table).
const SCORE_MATCH: i64 = 16;
const SCORE_GAP_START: i64 = -3;
const SCORE_GAP_EXTENSION: i64 = -1;
const BONUS_BOUNDARY: i64 = SCORE_MATCH / 2;
const BONUS_NON_WORD: i64 = SCORE_MATCH / 2;
const BONUS_CAMEL_123: i64 = BONUS_BOUNDARY + SCORE_GAP_EXTENSION;
const BONUS_CONSECUTIVE: i64 = -(SCORE_GAP_START + SCORE_GAP_EXTENSION);
const BONUS_FIRST_CHAR_MULTIPLIER: i64 = 2;

/// One scored match: the total score and the matched byte positions in the
/// haystack (ascending, one per query char).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuzzyMatch {
    pub score: i64,
    pub positions: Vec<usize>,
}

impl FuzzyMatch {
    /// Whether the match starts at the beginning of the haystack — the
    /// strongest single signal a miss hint has.
    pub fn starts_at_head(&self) -> bool {
        self.positions.first() == Some(&0)
    }
}

/// Match `query` as a subsequence of `haystack`, case-insensitively.
/// Returns `None` when no subsequence exists; an empty query matches with
/// score 0 and no positions.
pub fn fuzzy_match(query: &str, haystack: &str) -> Option<FuzzyMatch> {
    let q: Vec<char> = query.chars().collect();
    let h: Vec<char> = haystack.chars().collect();
    if q.is_empty() {
        return Some(FuzzyMatch {
            score: 0,
            positions: Vec::new(),
        });
    }
    if q.len() > h.len() {
        return None;
    }
    let lower: Vec<char> = h.iter().flat_map(|c| c.to_lowercase()).collect();
    // `to_lowercase` can expand one char into several (İ → i̇), which would
    // desynchronize positions. Reject that case rather than mis-report.
    if lower.len() != h.len() {
        return None;
    }
    let ql: Vec<char> = q.iter().flat_map(|c| c.to_lowercase()).collect();
    // Forward pass: the earliest index where the whole query completes.
    // Matching greedily forward is what makes the *existence* test exact —
    // a backward-only scan can walk past the only usable characters.
    let mut qi = 0usize;
    let mut end = None;
    for (i, c) in lower.iter().enumerate() {
        if qi < ql.len() && *c == ql[qi] {
            qi += 1;
            if qi == ql.len() {
                end = Some(i);
                break;
            }
        }
    }
    let end = end?;
    // Backward pass from that known-good end: the earliest start, so the
    // span is the tightest one ending there.
    let mut positions = vec![0usize; ql.len()];
    let mut k = ql.len();
    let mut upper = end + 1;
    while k > 0 {
        let mut found = None;
        let mut i = upper;
        while i > 0 {
            i -= 1;
            if lower[i] == ql[k - 1] {
                found = Some(i);
                break;
            }
        }
        let i = found?;
        positions[k - 1] = i;
        upper = i;
        k -= 1;
    }
    let score = score_positions(&h, &positions);
    Some(FuzzyMatch { score, positions })
}

/// The fzf bonus/penalty walk over a chosen match.
fn score_positions(h: &[char], positions: &[usize]) -> i64 {
    let mut score = 0i64;
    let mut prev: Option<usize> = None;
    for (qi, &i) in positions.iter().enumerate() {
        let mut bonus = char_bonus(h, i);
        // The first query character's boundary bonus counts double.
        if qi == 0 {
            bonus *= BONUS_FIRST_CHAR_MULTIPLIER;
        }
        score += SCORE_MATCH + bonus;
        if let Some(p) = prev {
            if i == p + 1 {
                score += BONUS_CONSECUTIVE;
            } else {
                // Gap: one start penalty plus an extension per skipped char.
                let gap = i - p - 1;
                score += SCORE_GAP_START + SCORE_GAP_EXTENSION * (gap as i64 - 1).max(0);
            }
        }
        prev = Some(i);
    }
    score
}

/// Boundary bonus for a match at index `i` (fzf's char-class rules).
fn char_bonus(h: &[char], i: usize) -> i64 {
    let cur = h[i];
    if i == 0 {
        return BONUS_BOUNDARY;
    }
    let prev = h[i - 1];
    let non_word = !prev.is_alphanumeric();
    if non_word {
        // A separator (`/`, `_`, `-`, space, `.`) is as good as a word head.
        return BONUS_NON_WORD;
    }
    let camel = prev.is_lowercase() && cur.is_uppercase();
    let digit_break = prev.is_alphabetic() != cur.is_alphabetic();
    if camel || digit_break {
        return BONUS_CAMEL_123;
    }
    0
}

/// Rank `candidates` by score for `query`: best first, ties broken by
/// shorter haystack (a tighter match of the same score is the better guess)
/// then by original order. Returns `(index, match)` pairs.
pub fn rank(query: &str, candidates: &[String]) -> Vec<(usize, FuzzyMatch)> {
    let mut scored: Vec<(usize, FuzzyMatch)> = candidates
        .iter()
        .enumerate()
        .filter_map(|(i, c)| fuzzy_match(query, c).map(|m| (i, m)))
        .collect();
    scored.sort_by(|a, b| {
        b.1.score
            .cmp(&a.1.score)
            .then_with(|| {
                candidates[a.0]
                    .chars()
                    .count()
                    .cmp(&candidates[b.0].chars().count())
            })
            .then_with(|| a.0.cmp(&b.0))
    });
    scored
}

/// The "did you mean …" line for a failed lookup: the best `limit` fuzzy
/// candidates (requiring a real subsequence — an unrelated candidate is
/// noise, not a hint). `None` when nothing matches.
pub fn miss_hint(query: &str, candidates: &[String], limit: usize) -> Option<String> {
    if limit == 0 {
        return None;
    }
    let ranked = rank(query, candidates);
    if ranked.is_empty() {
        return None;
    }
    let names: Vec<&str> = ranked
        .iter()
        .take(limit)
        .map(|(i, _)| candidates[*i].as_str())
        .collect();
    Some(format!("did you mean: {}?", names.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn matches_subsequences_case_insensitively() {
        let m = fuzzy_match("fzrs", "src/fuzzy.rs").expect("subsequence");
        // Greedy-from-the-back window (fzf V1's selection): the last
        // occurrence of each query char, walking leftwards.
        assert_eq!(m.positions, vec![4, 7, 10, 11]);
        assert!(m.score > 0, "a boundary-rich match scores positive: {m:?}");
        assert!(!m.starts_at_head());
        assert!(fuzzy_match("FZRS", "src/fuzzy.rs").is_some());
        assert!(fuzzy_match("fzrs", "Fuzzy.RS").is_some());
        // A cross-boundary subsequence still matches (`src` then `fz`).
        assert!(fuzzy_match("srcfz", "src/fuzzy.rs").is_some());
        // Order is strict: `z` at index 6 has no `f` after it.
        assert!(fuzzy_match("zf", "src/fuzzy.rs").is_none());
        // And a character that appears once cannot match twice.
        assert!(fuzzy_match("yy", "src/fuzzy.rs").is_none());
        // `rsfz` is NOT a subsequence: the only `s` after the `r` is the
        // final one, which leaves nothing for `f` and `z`.
        assert!(fuzzy_match("rsfz", "src/fuzzy.rs").is_none());
        assert!(fuzzy_match("longer-than-haystack", "tiny").is_none());
        // Empty query trivially matches.
        let e = fuzzy_match("", "anything").unwrap();
        assert_eq!(e.score, 0);
        assert!(e.positions.is_empty());
    }

    #[test]
    fn boundary_and_head_matches_outrank_scattered_ones() {
        let boundary = fuzzy_match("fb", "foo_bar").unwrap();
        let scattered = fuzzy_match("fb", "xxfxxbxx").unwrap();
        assert!(
            boundary.score > scattered.score,
            "word boundaries beat scatter: {boundary:?} vs {scattered:?}"
        );
        let head = fuzzy_match("foo", "foobar").unwrap();
        assert!(head.starts_at_head());
        assert!(head.positions == vec![0, 1, 2], "consecutive run: {head:?}");
        let camel = fuzzy_match("fb", "fooBar").unwrap();
        assert!(camel.score > scattered.score, "camel transition bonus");
        // Consecutive matches pay no gap penalty.
        let consec = fuzzy_match("abc", "abcdef").unwrap();
        let gapped = fuzzy_match("abc", "axbxcx").unwrap();
        assert!(consec.score > gapped.score, "{consec:?} vs {gapped:?}");
    }

    #[test]
    fn rank_and_hints_are_deterministic_and_relevant() {
        let cands = s(&[
            "src/fuzzy.rs",
            "tests/fuzzy_old.rs",
            "src/glob.rs",
            "README.md",
        ]);
        let ranked = rank("fzrs", &cands);
        assert_eq!(ranked[0].0, 0, "the tightest match wins: {ranked:?}");
        assert_eq!(ranked[1].0, 1);
        assert_eq!(ranked.len(), 2, "non-matches are not ranked: {ranked:?}");
        // Deterministic across runs (no map iteration involved).
        assert_eq!(rank("fzrs", &cands), ranked);

        let hint = miss_hint("fzrs", &cands, 2).unwrap();
        assert!(hint.starts_with("did you mean: "), "{hint}");
        assert!(hint.contains("src/fuzzy.rs"), "{hint}");
        assert!(hint.ends_with('?'));
        assert!(!hint.contains("README"), "only subsequence matches hint");
        assert!(miss_hint("zzzz", &cands, 2).is_none(), "no noise hints");
        assert!(miss_hint("fzrs", &[], 2).is_none());
        assert!(miss_hint("fzrs", &cands, 0).is_none());
    }

    #[test]
    fn a_wider_gap_scores_lower_and_ranks_second() {
        // Two candidates with the same shape: the one with the wider gap
        // pays the extra gap-extension penalty and loses the ranking.
        let cands = s(&["a_x", "a__x"]);
        let ranked = rank("ax", &cands);
        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].0, 0, "the tighter match wins: {ranked:?}");
        assert!(
            ranked[0].1.score > ranked[1].1.score,
            "gap extension must cost: {ranked:?}"
        );
    }
}
