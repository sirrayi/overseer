//! Response-cache tier 0: exact matches (GPTCache's first tier, arsenal B2).
//!
//! GPTCache layers a cache: an exact string match first, then a vector
//! similarity tier, then an LLM-judged tier. This port lands **tier 0 only**
//! — the one that cannot be wrong. A hit requires a byte-identical
//! `(model, prompt)` pair, so a cached answer is by construction the answer
//! the provider already gave for that exact request.
//!
//! Design notes:
//! - The key is a digest of `model \0 prompt` (a NUL separator, because a
//!   prompt may contain anything else). Model is in the key: the same prompt
//!   to a different model is a different request.
//! - The store is bounded and evicts least-recently-used; a cache that grows
//!   without bound is a memory leak wearing a performance hat.
//! - Callers must only consult the cache for deterministic requests. The
//!   engine has no temperature knob today (every request is deterministic),
//!   which is exactly why tier 0 is honest here.
//! - No persistence: a session's cache dies with the session. On-disk
//!   caching would need a key over the whole request (tools, budget,
//!   system segments), which is a provider-layer decision, not this one.
//!
//! `// DEFERRED(owner): tier 1 (embedding similarity) and tier 2 (judged)
//! caching, plus an on-disk store — tier 0 lands because it is provably
//! correct; the other tiers need a similarity model this batch excludes.`

use std::collections::HashMap;

/// One cached response plus its bookkeeping.
#[derive(Debug, Clone)]
struct Entry {
    response: String,
    hits: u64,
    /// Monotonic use counter — the LRU order (no timestamps: a counter is
    /// deterministic and immune to clock skew inside a test).
    last_used: u64,
}

/// Bounded exact-match response cache.
#[derive(Debug)]
pub struct ExactCache {
    capacity: usize,
    entries: HashMap<String, Entry>,
    clock: u64,
    hits: u64,
    misses: u64,
}

impl ExactCache {
    /// A cache holding at most `capacity` entries (0 = disabled: every
    /// lookup misses and nothing is stored).
    pub fn new(capacity: usize) -> Self {
        ExactCache {
            capacity,
            entries: HashMap::new(),
            clock: 0,
            hits: 0,
            misses: 0,
        }
    }

    /// The tier-0 key: `sha256(model \0 prompt)`. Public so a caller can
    /// log/compare keys without touching the store.
    pub fn key(model: &str, prompt: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        h.update(model.as_bytes());
        h.update([0u8]);
        h.update(prompt.as_bytes());
        format!("{:x}", h.finalize())
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// (hits, misses) — the caller records these in the ledger.
    pub fn stats(&self) -> (u64, u64) {
        (self.hits, self.misses)
    }

    /// Look up an exact key. A hit counts as a use (LRU).
    pub fn get(&mut self, key: &str) -> Option<String> {
        self.clock += 1;
        let clock = self.clock;
        match self.entries.get_mut(key) {
            Some(e) => {
                e.hits += 1;
                e.last_used = clock;
                self.hits += 1;
                Some(e.response.clone())
            }
            None => {
                self.misses += 1;
                None
            }
        }
    }

    /// Store a response. Evicts the least-recently-used entry when full.
    /// Re-storing a key overwrites (the newest answer for a request wins).
    pub fn put(&mut self, key: &str, response: &str) {
        if self.capacity == 0 {
            return;
        }
        self.clock += 1;
        if self.entries.contains_key(key) {
            self.entries.insert(
                key.to_string(),
                Entry {
                    response: response.to_string(),
                    hits: self.entries.get(key).map(|e| e.hits).unwrap_or(0),
                    last_used: self.clock,
                },
            );
            return;
        }
        if self.entries.len() >= self.capacity {
            // Deterministic eviction: oldest use wins the exit; ties (two
            // entries inserted in the same tick) break on key, so eviction
            // never depends on HashMap iteration order.
            if let Some(victim) = self
                .entries
                .iter()
                .min_by(|a, b| a.1.last_used.cmp(&b.1.last_used).then_with(|| a.0.cmp(b.0)))
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&victim);
            }
        }
        self.entries.insert(
            key.to_string(),
            Entry {
                response: response.to_string(),
                hits: 0,
                last_used: self.clock,
            },
        );
    }

    /// Convenience: key + get in one step (the provider-call shape).
    pub fn get_for(&mut self, model: &str, prompt: &str) -> Option<String> {
        let k = Self::key(model, prompt);
        self.get(&k)
    }

    /// Convenience: key + put.
    pub fn put_for(&mut self, model: &str, prompt: &str, response: &str) {
        let k = Self::key(model, prompt);
        self.put(&k, response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_stable_and_model_scoped() {
        let a = ExactCache::key("claude-sonnet-5", "hello");
        let b = ExactCache::key("claude-sonnet-5", "hello");
        assert_eq!(a, b, "the same request must hash the same");
        assert_ne!(
            a,
            ExactCache::key("fleet-k3", "hello"),
            "the same prompt to another model is another request"
        );
        assert_ne!(a, ExactCache::key("claude-sonnet-5", "hello!"));
        // A NUL separator stops field-boundary collisions.
        assert_ne!(
            ExactCache::key("a", "bc"),
            ExactCache::key("a\u{0}b", "c"),
            "separator keeps (model, prompt) unambiguous"
        );
        assert_eq!(a.len(), 64, "sha256 hex");
    }

    #[test]
    fn exact_hits_only_and_stats_are_counted() {
        let mut c = ExactCache::new(4);
        assert!(c.get_for("m", "p").is_none());
        c.put_for("m", "p", "the answer");
        assert_eq!(c.get_for("m", "p").as_deref(), Some("the answer"));
        // A near-miss is a miss: tier 0 is exact by construction.
        assert!(c.get_for("m", "p ").is_none());
        assert!(c.get_for("other", "p").is_none());
        assert_eq!(c.stats(), (1, 3));
        assert_eq!(c.len(), 1);
    }

    #[test]
    fn capacity_evicts_the_least_recently_used() {
        let mut c = ExactCache::new(2);
        c.put_for("m", "one", "1");
        c.put_for("m", "two", "2");
        // Touch `one` so `two` becomes the coldest.
        assert_eq!(c.get_for("m", "one").as_deref(), Some("1"));
        c.put_for("m", "three", "3");
        assert_eq!(c.len(), 2, "capacity is a hard bound");
        assert_eq!(c.get_for("m", "two"), None, "two was evicted");
        assert_eq!(c.get_for("m", "one").as_deref(), Some("1"));
        assert_eq!(c.get_for("m", "three").as_deref(), Some("3"));
        // Re-storing refreshes the value and does not grow the store.
        c.put_for("m", "one", "1b");
        assert_eq!(c.get_for("m", "one").as_deref(), Some("1b"));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn zero_capacity_is_disabled_not_broken() {
        let mut c = ExactCache::new(0);
        c.put_for("m", "p", "x");
        assert!(c.is_empty());
        assert!(c.get_for("m", "p").is_none());
        assert_eq!(c.stats(), (0, 1));
    }
}
