//! ACT-R base-level activation (Petrov 2006 hybrid) over a per-store use
//! journal.
//!
//! Each use appends `{"n":"layer/name.md","t":<epoch_s>}` to
//! `<store>/.index/uses.jsonl` (O_APPEND: concurrent sessions interleave
//! whole lines). Loading folds the journal — and the compacted
//! `.index/activation.json` that `compact` writes — into [`Uses`]: the use
//! count, the first use and the three most recent.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

pub const JOURNAL: &str = ".index/uses.jsonl";
pub const COMPACTED: &str = ".index/activation.json";

/// Uses kept exactly; older ones are approximated (Petrov's k).
const K: usize = 3;
/// Decay exponent (ACT-R's standard d).
const D: f64 = 0.5;
/// Age floor in days (one hour): a use "now" must not score infinity.
const FLOOR_DAYS: f64 = 1.0 / 24.0;

/// One note's folded use history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uses {
    pub n: u64,
    pub first: u64,
    /// The most recent uses, newest first, at most `K`.
    pub last3: Vec<u64>,
}

impl Uses {
    /// A single use at `t` (a never-used note counts as one use at its mtime).
    pub fn once(t: u64) -> Uses {
        Uses {
            n: 1,
            first: t,
            last3: vec![t],
        }
    }

    pub(crate) fn add(&mut self, t: u64) {
        self.n += 1;
        self.first = self.first.min(t);
        let at = self
            .last3
            .iter()
            .position(|&x| x < t)
            .unwrap_or(self.last3.len());
        self.last3.insert(at, t);
        self.last3.truncate(K);
    }
}

/// Append one use of `rel` at `t` to the store's journal. A symlinked
/// `.index/` errors (the store never follows one); callers treat the
/// journal as best-effort, so the refusal never fails a recall.
pub fn record(store: &Path, rel: &str, t: u64) -> std::io::Result<()> {
    let idx = super::real_dir(store, ".index").map_err(std::io::Error::other)?;
    crate::harden::ensure_private_dir(&idx)?;
    let mut f = crate::tools::open_append_no_follow(&store.join(JOURNAL))?;
    let line = serde_json::json!({ "n": rel, "t": t });
    f.write_all(format!("{line}\n").as_bytes())
}

/// Folded history per note (`layer/name.md`) — compacted state plus the
/// journal. Unreadable files and malformed lines contribute nothing.
pub fn load(store: &Path) -> BTreeMap<String, Uses> {
    // A symlinked `.index/` reads as empty — the store never follows one.
    if super::real_dir(store, ".index").is_err() {
        return BTreeMap::new();
    }
    let mut out: BTreeMap<String, Uses> = crate::tools::read_no_follow(&store.join(COMPACTED))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    #[derive(Deserialize)]
    struct Line {
        n: String,
        t: u64,
    }
    let journal = crate::tools::read_no_follow(&store.join(JOURNAL)).unwrap_or_default();
    for l in journal.lines() {
        let Ok(Line { n, t }) = serde_json::from_str::<Line>(l) else {
            continue;
        };
        match out.get_mut(&n) {
            Some(u) => u.add(t),
            None => {
                out.insert(n, Uses::once(t));
            }
        }
    }
    out
}

/// Fold the journal into `activation.json` and truncate it. Returns the
/// number of notes tracked. Single writer: the store lock makes the
/// fold+truncate one mutation, so a use appended between them is never
/// lost to a race (a use arriving later just lands in the next fold).
/// A symlinked `.index/` refuses — the fold would otherwise rewrite a
/// file outside the store.
pub fn compact(store: &Path) -> std::io::Result<usize> {
    let _lock = super::StoreLock::acquire(store)?;
    let folded = load(store);
    super::real_dir(store, ".index").map_err(std::io::Error::other)?;
    crate::harden::ensure_private_dir(&store.join(".index"))?;
    super::store_write(
        store,
        COMPACTED,
        serde_json::to_string(&folded)
            .map_err(std::io::Error::other)?
            .as_bytes(),
    )
    .map_err(std::io::Error::other)?;
    super::store_write(store, JOURNAL, b"").map_err(std::io::Error::other)?;
    Ok(folded.len())
}

/// Petrov's hybrid base-level activation B at clock `now` (epoch s), k = 3,
/// d = 0.5. Ages are in days, floored at one hour. Up to k uses sum
/// exactly; beyond, the n − k older uses are approximated as spread evenly
/// between the k-th most recent and the first:
/// `B = ln(Σ_{i≤k} t_i^−d + (n−k)(t_n^{1−d} − t_k^{1−d}) / ((1−d)(t_n − t_k)))`,
/// falling back to the exact partial sum when `t_n ≤ t_k`.
pub fn base_level(u: &Uses, now: u64) -> f64 {
    let age = |t: u64| (now.saturating_sub(t) as f64 / 86_400.0).max(FLOOR_DAYS);
    let exact: f64 = u.last3.iter().map(|&t| age(t).powf(-D)).sum();
    if u.n <= K as u64 || u.last3.len() < K {
        return exact.ln();
    }
    let (tn, tk) = (age(u.first), age(u.last3[K - 1]));
    if tn <= tk {
        return exact.ln();
    }
    let tail =
        (u.n - K as u64) as f64 * (tn.powf(1.0 - D) - tk.powf(1.0 - D)) / ((1.0 - D) * (tn - tk));
    (exact + tail).ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 86_400;

    /// Evenly spaced uses: first 100 days ago, last one day ago.
    fn history(n: u64, now: u64) -> (Uses, Vec<u64>) {
        let span = 99 * DAY;
        let times: Vec<u64> = (0..n)
            .map(|i| now - 100 * DAY + span * i / (n - 1))
            .collect();
        let mut u = Uses::once(times[0]);
        for &t in &times[1..] {
            u.add(t);
        }
        (u, times)
    }

    #[test]
    fn petrov_hybrid_tracks_the_exact_sum() {
        let now = 1_900_000_000;
        for n in [5, 20, 200] {
            let (u, times) = history(n, now);
            let exact: f64 = times
                .iter()
                .map(|&t| (((now - t) as f64 / DAY as f64).max(FLOOR_DAYS)).powf(-D))
                .sum::<f64>()
                .ln();
            let b = base_level(&u, now);
            assert!(
                (b - exact).abs() < 0.15,
                "n={n}: hybrid {b} vs exact {exact}"
            );
        }
    }

    #[test]
    fn few_uses_are_exact_and_recent_outranks_stale() {
        let now = 1_900_000_000;
        let u = Uses::once(now - 4 * DAY);
        assert!((base_level(&u, now) - (4f64).powf(-D).ln()).abs() < 1e-12);
        // A use "now" is floored at one hour, not infinite.
        assert!(base_level(&Uses::once(now), now).is_finite());
        assert!(
            base_level(&Uses::once(now - DAY), now) > base_level(&Uses::once(now - 30 * DAY), now)
        );
    }

    #[test]
    fn journal_appends_fold_and_compact() {
        let dir = std::env::temp_dir().join(format!("ov-act-{}", uuid::Uuid::now_v7()));
        for t in [10, 40, 20, 30] {
            record(&dir, "semantic/a.md", t).unwrap();
        }
        record(&dir, "semantic/b.md", 5).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(JOURNAL))
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        let want = Uses {
            n: 4,
            first: 10,
            last3: vec![40, 30, 20],
        };
        assert_eq!(load(&dir)["semantic/a.md"], want);
        assert_eq!(compact(&dir).unwrap(), 2);
        assert_eq!(std::fs::read_to_string(dir.join(JOURNAL)).unwrap(), "");
        record(&dir, "semantic/a.md", 50).unwrap();
        let after = &load(&dir)["semantic/a.md"];
        assert_eq!(
            (after.n, after.first, after.last3.clone()),
            (5, 10, vec![50, 40, 30])
        );
    }
}
