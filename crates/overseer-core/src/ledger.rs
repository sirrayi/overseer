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

pub struct Ledger {
    file: File,
    path: PathBuf,
    pub total_cost_usd: f64,
    pub calls: u64,
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
        })
    }

    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        let file = OpenOptions::new().append(true).open(&path)?;
        // Tally existing rows so a resumed session keeps a running total.
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let mut total = 0.0;
        let mut calls = 0u64;
        for line in existing.lines() {
            if let Ok(r) = serde_json::from_str::<UsageRecord>(line) {
                total += r.cost_usd;
                calls += 1;
            }
        }
        Ok(Ledger {
            file,
            path,
            total_cost_usd: total,
            calls,
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
        Ok(())
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
