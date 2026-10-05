//! Usage ledger (deliverable 0.5; playbook Ch.6 §6.4 instrumentation plan).
//! Per-call record to `ledger.jsonl` — every efficiency feature depends on
//! this telemetry existing from the first API call.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::event::now_ms;
use crate::ir::Usage;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UsageRecord {
    pub ts_ms: u64,
    pub model: String,
    pub fresh_input: u64,
    pub cache_write: u64,
    pub cache_read: u64,
    pub output: u64,
    pub reasoning: u64,
    /// Serialized request size — tracks context growth per turn.
    pub request_bytes: u64,
    pub latency_ms: u64,
    pub tool_calls: u32,
    pub cost_usd: f64,
    /// cache_read / total_input — the SEV-metric KPI (target ≥0.90 in-session).
    pub cache_hit_rate: f64,
}

impl UsageRecord {
    pub fn from_usage(
        model: &str,
        u: &Usage,
        request_bytes: u64,
        latency_ms: u64,
        tool_calls: u32,
        cost_usd: f64,
    ) -> Self {
        let total_in = u.total_input();
        UsageRecord {
            ts_ms: now_ms(),
            model: model.to_string(),
            fresh_input: u.fresh_input,
            cache_write: u.cache_write,
            cache_read: u.cache_read,
            output: u.output,
            reasoning: u.reasoning,
            request_bytes,
            latency_ms,
            tool_calls,
            cost_usd,
            cache_hit_rate: if total_in > 0 {
                u.cache_read as f64 / total_in as f64
            } else {
                0.0
            },
        }
    }
}

/// Session token totals by cache class — the numbers a dashboard needs to
/// show how much of the prompt was served from the provider cache.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct CacheStats {
    pub fresh_input: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Billed output tokens (visible output plus reasoning).
    pub output: u64,
}

impl CacheStats {
    /// `cache_read / (fresh_input + cache_read + cache_write)`; 0.0 when no
    /// input has been billed yet.
    pub fn hit_rate(&self) -> f64 {
        let denom = self
            .fresh_input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write);
        if denom == 0 {
            0.0
        } else {
            self.cache_read as f64 / denom as f64
        }
    }

    /// `self - earlier` per field, clamped at zero — the per-run slice a
    /// RunEnd event reports when `earlier` is the run-start snapshot.
    pub fn saturating_delta(&self, earlier: &CacheStats) -> CacheStats {
        CacheStats {
            fresh_input: self.fresh_input.saturating_sub(earlier.fresh_input),
            cache_read: self.cache_read.saturating_sub(earlier.cache_read),
            cache_write: self.cache_write.saturating_sub(earlier.cache_write),
            output: self.output.saturating_sub(earlier.output),
        }
    }

    /// Total input side (fresh + read + write) — the `> 0` test frontends
    /// use before showing a hit rate.
    pub fn input_total(&self) -> u64 {
        self.fresh_input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
    }

    fn add(&mut self, r: &UsageRecord) {
        self.fresh_input = self.fresh_input.saturating_add(r.fresh_input);
        self.cache_read = self.cache_read.saturating_add(r.cache_read);
        self.cache_write = self.cache_write.saturating_add(r.cache_write);
        self.output = self
            .output
            .saturating_add(r.output.saturating_add(r.reasoning));
    }
}

pub struct Ledger {
    file: File,
    path: PathBuf,
    pub total_cost_usd: f64,
    pub calls: u64,
    cache: CacheStats,
}

impl Ledger {
    pub fn create(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(&path)?;
        Ok(Ledger {
            file,
            path,
            total_cost_usd: 0.0,
            calls: 0,
            cache: CacheStats::default(),
        })
    }

    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().append(true).open(&path)?;
        // Tally existing rows so a resumed session keeps a running total.
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut total = 0.0;
        let mut calls = 0u64;
        let mut cache = CacheStats::default();
        for line in existing.lines() {
            if let Ok(r) = serde_json::from_str::<UsageRecord>(line) {
                total += r.cost_usd;
                calls += 1;
                cache.add(&r);
            }
        }
        Ok(Ledger {
            file,
            path,
            total_cost_usd: total,
            calls,
            cache,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&mut self, rec: UsageRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(&rec).map_err(std::io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.total_cost_usd += rec.cost_usd;
        self.calls += 1;
        self.cache.add(&rec);
        Ok(())
    }

    /// Running cache-class token totals for this session (resumed sessions
    /// include the rows already in `ledger.jsonl`).
    pub fn cache_stats(&self) -> CacheStats {
        self.cache
    }

    /// Read every record in a ledger file (for the stats/dashboard path —
    /// tolerates a torn tail like the event log).
    pub fn read_all(path: impl AsRef<Path>) -> Vec<UsageRecord> {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        text.lines()
            .filter_map(|l| serde_json::from_str::<UsageRecord>(l).ok())
            .collect()
    }

    /// Session-level aggregates: the cache-hit-rate KPI (Σcache_read /
    /// Σtotal_input — the SEV metric, target ≥0.90 in-session).
    pub fn summarize(records: &[UsageRecord]) -> Summary {
        let mut s = Summary::default();
        for r in records {
            // FAIL-3: saturating accumulation — a corrupt/hand-edited ledger
            // row (u64::MAX fields) must not panic the dashboard.
            s.calls += 1;
            s.input_tokens = s.input_tokens.saturating_add(
                r.fresh_input
                    .saturating_add(r.cache_write)
                    .saturating_add(r.cache_read),
            );
            s.cache_read_tokens = s.cache_read_tokens.saturating_add(r.cache_read);
            s.output_tokens = s
                .output_tokens
                .saturating_add(r.output.saturating_add(r.reasoning));
            s.total_cost_usd += r.cost_usd;
            s.latency_ms = s.latency_ms.saturating_add(r.latency_ms);
        }
        if s.input_tokens > 0 {
            s.cache_hit_rate = s.cache_read_tokens as f64 / s.input_tokens as f64;
        }
        s.cache_alert = s.input_tokens > 0 && s.cache_hit_rate < 0.90;
        s
    }
}

/// Aggregates over a ledger file — the session's efficiency dashboard.
#[derive(Debug, Default)]
pub struct Summary {
    pub calls: usize,
    pub input_tokens: u64,
    pub cache_read_tokens: u64,
    pub output_tokens: u64,
    pub total_cost_usd: f64,
    /// Σcache_read / Σtotal_input — the SEV metric (target ≥0.90).
    pub cache_hit_rate: f64,
    /// B1-10: true when `cache_hit_rate < 0.90` — the prefix-discipline
    /// alert. Computed in `summarize`, read by dashboards/CI.
    pub cache_alert: bool,
    pub latency_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(fresh: u64, read: u64, write: u64, output: u64) -> UsageRecord {
        UsageRecord::from_usage(
            "m",
            &Usage {
                fresh_input: fresh,
                cache_read: read,
                cache_write: write,
                output,
                ..Default::default()
            },
            0,
            0,
            0,
            0.0,
        )
    }

    #[test]
    fn cache_stats_delta_is_per_run_and_clamped() {
        let end = CacheStats {
            fresh_input: 100,
            cache_read: 500,
            cache_write: 40,
            output: 20,
        };
        let start = CacheStats {
            fresh_input: 30,
            cache_read: 200,
            cache_write: 40,
            output: 8,
        };
        assert_eq!(
            end.saturating_delta(&start),
            CacheStats {
                fresh_input: 70,
                cache_read: 300,
                cache_write: 0,
                output: 12,
            }
        );
        // A ledger reopen anomaly (end < start) clamps to zero, never
        // wraps around to a huge count.
        assert_eq!(
            start.saturating_delta(&end),
            CacheStats {
                fresh_input: 0,
                cache_read: 0,
                cache_write: 0,
                output: 0,
            }
        );
        assert_eq!(start.input_total(), 270);
        assert_eq!(CacheStats::default().input_total(), 0);
    }

    #[test]
    fn cache_hit_rate_zero_denominator_is_zero() {
        assert_eq!(CacheStats::default().hit_rate(), 0.0);
        let s = CacheStats {
            fresh_input: 10,
            cache_read: 60,
            cache_write: 30,
            output: 5,
        };
        assert!((s.hit_rate() - 0.6).abs() < 1e-9);
    }

    #[test]
    fn cache_stats_accumulate_and_survive_reopen() {
        let dir = std::env::temp_dir().join(format!("overseer-ledger-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        let mut l = Ledger::create(&path).unwrap();
        l.record(rec(100, 0, 900, 7)).unwrap();
        l.record(rec(50, 900, 0, 3)).unwrap();
        let want = CacheStats {
            fresh_input: 150,
            cache_read: 900,
            cache_write: 900,
            output: 10,
        };
        assert_eq!(l.cache_stats(), want);
        drop(l);
        assert_eq!(Ledger::open(&path).unwrap().cache_stats(), want);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summarize_saturates_on_overflow() {
        // FAIL-3: u64::MAX ledger rows must not panic.
        let rec = UsageRecord {
            ts_ms: 0,
            model: "m".into(),
            fresh_input: u64::MAX,
            cache_write: u64::MAX,
            cache_read: u64::MAX,
            output: u64::MAX,
            reasoning: u64::MAX,
            request_bytes: 0,
            latency_ms: u64::MAX,
            tool_calls: 0,
            cost_usd: 0.0,
            cache_hit_rate: 0.0,
        };
        let s = Ledger::summarize(&[rec]);
        assert_eq!(s.input_tokens, u64::MAX);
        assert_eq!(s.output_tokens, u64::MAX);
    }

    #[test]
    fn cache_alert_threshold() {
        // B1-10: alert fires below 0.90, silent at/above.
        let low = vec![UsageRecord {
            ts_ms: 0,
            model: "m".into(),
            fresh_input: 50,
            cache_write: 0,
            cache_read: 10,
            output: 0,
            reasoning: 0,
            request_bytes: 0,
            latency_ms: 0,
            tool_calls: 0,
            cost_usd: 0.0,
            cache_hit_rate: 0.0,
        }];
        let s = Ledger::summarize(&low);
        assert!(s.cache_alert, "0.167 hit rate must alert");
        let high = vec![UsageRecord {
            ts_ms: 0,
            model: "m".into(),
            fresh_input: 5,
            cache_write: 0,
            cache_read: 95,
            output: 0,
            reasoning: 0,
            request_bytes: 0,
            latency_ms: 0,
            tool_calls: 0,
            cost_usd: 0.0,
            cache_hit_rate: 0.0,
        }];
        let s2 = Ledger::summarize(&high);
        assert!(!s2.cache_alert, "0.95 hit rate must not alert");
    }
}
