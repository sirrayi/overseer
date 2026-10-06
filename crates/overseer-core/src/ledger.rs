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
    /// Set on a subagent settlement row: the task id whose own ledger
    /// total this row brings into the parent's. Zero tokens — the parent's
    /// token stats count only its own calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent: Option<String>,
    /// Why this call ran when it isn't an ordinary turn call —
    /// `memory_review` for the §1.7 learning review. Absent on rows from
    /// before the tag existed (old ledgers replay unchanged).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub purpose: Option<String>,
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
            subagent: None,
            purpose: None,
        }
    }

    /// A settlement row: `cost_usd` of subagent `task_id`'s spend, no tokens.
    pub fn settlement(task_id: &str, model: &str, cost_usd: f64) -> Self {
        UsageRecord {
            subagent: Some(task_id.to_string()),
            ..Self::from_usage(model, &Usage::default(), 0, 0, 0, cost_usd)
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
    /// Own calls plus settled subagent spend.
    pub total_cost_usd: f64,
    /// The settled-subagent share of `total_cost_usd`.
    pub subagent_cost_usd: f64,
    /// The learning-review share (`purpose: memory_review` rows).
    pub review_cost_usd: f64,
    /// Own provider calls (settlement rows excluded).
    pub calls: u64,
    cache: CacheStats,
    /// Per task id: spend settled so far (replayed on open, so resumes
    /// settle only the delta).
    settled: std::collections::HashMap<String, f64>,
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
            subagent_cost_usd: 0.0,
            review_cost_usd: 0.0,
            calls: 0,
            cache: CacheStats::default(),
            settled: Default::default(),
        })
    }

    pub fn open(path: impl AsRef<Path>) -> std::io::Result<Self> {
        let path = path.as_ref().to_path_buf();
        crate::harden::repair_torn_tail(&path)?;
        let file = OpenOptions::new().append(true).open(&path)?;
        let mut ledger = Ledger {
            file,
            path,
            total_cost_usd: 0.0,
            subagent_cost_usd: 0.0,
            review_cost_usd: 0.0,
            calls: 0,
            cache: CacheStats::default(),
            settled: Default::default(),
        };
        // Tally existing rows so a resumed session keeps a running total.
        let existing = std::fs::read_to_string(&ledger.path).unwrap_or_default();
        for line in existing.lines() {
            if let Ok(r) = serde_json::from_str::<UsageRecord>(line) {
                ledger.tally(&r);
            }
        }
        Ok(ledger)
    }

    fn tally(&mut self, r: &UsageRecord) {
        self.total_cost_usd += r.cost_usd;
        match &r.subagent {
            Some(id) => {
                self.subagent_cost_usd += r.cost_usd;
                *self.settled.entry(id.clone()).or_default() += r.cost_usd;
            }
            None => {
                self.calls += 1;
                self.cache.add(r);
                if r.purpose.as_deref() == Some(REVIEW_PURPOSE) {
                    self.review_cost_usd += r.cost_usd;
                }
            }
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn record(&mut self, rec: UsageRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(&rec).map_err(std::io::Error::other)?;
        line.push('\n');
        self.file.write_all(line.as_bytes())?;
        self.file.flush()?;
        self.tally(&rec);
        Ok(())
    }

    /// Bring subagent `task_id`'s spend to `total_usd` (its own ledger
    /// total): appends one settlement row for the unsettled delta, none
    /// when already settled — idempotent across drains and resumes.
    /// Returns the delta recorded.
    pub fn settle(&mut self, task_id: &str, model: &str, total_usd: f64) -> std::io::Result<f64> {
        let delta = total_usd - self.settled.get(task_id).copied().unwrap_or(0.0);
        if delta <= 1e-12 {
            return Ok(0.0);
        }
        self.record(UsageRecord::settlement(task_id, model, delta))?;
        Ok(delta)
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
            if r.subagent.is_some() {
                s.total_cost_usd += r.cost_usd;
                s.subagent_cost_usd += r.cost_usd;
                continue;
            }
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
    /// Own calls plus settled subagent spend.
    pub total_cost_usd: f64,
    /// The settled-subagent share of `total_cost_usd`.
    pub subagent_cost_usd: f64,
    /// Σcache_read / Σtotal_input — the SEV metric (target ≥0.90).
    pub cache_hit_rate: f64,
    /// B1-10: true when `cache_hit_rate < 0.90` — the prefix-discipline
    /// alert. Computed in `summarize`, read by dashboards/CI.
    pub cache_alert: bool,
    pub latency_ms: u64,
}

/// `purpose` of the §1.7 learning review's rows.
pub const REVIEW_PURPOSE: &str = "memory_review";
/// Below this many affordable output tokens the gate refuses a call.
pub const MIN_OUTPUT_TOKENS: u32 = 1_024;
/// Input-estimate stand-in for one image block.
const IMAGE_TOKENS: u64 = 1_600;

/// Counted input tokens of `req`: system, tool specs and message text,
/// images at a flat [`IMAGE_TOKENS`].
pub fn request_tokens(req: &crate::provider::Request<'_>) -> u64 {
    use crate::ir::Block;
    let mut text = String::new();
    let mut images = 0u64;
    for s in req.system {
        text.push_str(&s.text);
    }
    for t in req.tools {
        text.push_str(&t.name);
        text.push_str(&t.description);
        text.push_str(&t.input_schema.to_string());
    }
    for m in req.messages {
        for b in &m.content {
            match b {
                Block::Text { text: t } => text.push_str(t),
                Block::ToolResult { content, .. } => text.push_str(content),
                Block::ToolCall { name, input, .. } => {
                    text.push_str(name);
                    text.push_str(&input.to_string());
                }
                Block::Reasoning { raw } => text.push_str(&raw.to_string()),
                Block::Image { .. } => images += 1,
            }
        }
    }
    crate::tokens::count_tokens(&text, req.model)
        .saturating_add(images.saturating_mul(IMAGE_TOKENS))
}

/// The one spend gate every provider call goes through (main turn,
/// reflection, aux, memory review, consult; subagents and escalations
/// are agents, so theirs too). Before the call: `worst` = the counted
/// input priced at the dearer of fresh/cache-write plus `max_tokens` of
/// output; when `spent + reserved + worst` would pass the cap the
/// request's `max_tokens` drops to what is left, and below
/// [`MIN_OUTPUT_TOKENS`] the call is refused ([`GateError::Budget`]).
/// After the call — answered or failed — exactly one ledger row, tagged
/// with `purpose` (None = an ordinary turn call).
pub struct Gate<'a> {
    pub provider: &'a dyn crate::provider::Provider,
    pub ledger: &'a mut Ledger,
    pub cap_usd: f64,
    /// Caps still held for subagents in flight.
    pub reserved_usd: f64,
}

#[derive(Debug)]
pub enum GateError {
    /// Not even [`MIN_OUTPUT_TOKENS`] fit in what is left.
    Budget {
        worst_usd: f64,
        remaining_usd: f64,
    },
    Provider(crate::provider::ProviderError),
    /// The ledger row could not be written.
    Io(std::io::Error),
}

impl std::fmt::Display for GateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GateError::Budget {
                worst_usd,
                remaining_usd,
            } => write!(
                f,
                "budget: the call could cost ${worst_usd:.4}, ${remaining_usd:.4} left"
            ),
            GateError::Provider(e) => write!(f, "{e}"),
            GateError::Io(e) => write!(f, "ledger: {e}"),
        }
    }
}

impl Gate<'_> {
    /// Gate, send and ledger one call. Returns the response and its cost.
    pub fn call(
        self,
        req: &mut crate::provider::Request<'_>,
        purpose: Option<&str>,
    ) -> Result<(crate::provider::Response, f64), GateError> {
        let profile = crate::profile::lookup(req.model);
        let n = request_tokens(req);
        let in_usd = profile
            .cost_usd(&Usage {
                fresh_input: n,
                ..Usage::default()
            })
            .max(profile.cost_usd(&Usage {
                cache_write: n,
                ..Usage::default()
            }));
        let per_out = profile.cost_usd(&Usage {
            output: 1_000_000,
            ..Usage::default()
        }) / 1_000_000.0;
        let remaining = self.cap_usd - self.ledger.total_cost_usd - self.reserved_usd;
        let worst = in_usd + f64::from(req.max_tokens) * per_out;
        if worst > remaining {
            let afford = if per_out > 0.0 {
                ((remaining - in_usd) / per_out).floor()
            } else {
                -1.0
            };
            if afford < f64::from(MIN_OUTPUT_TOKENS) {
                return Err(GateError::Budget {
                    worst_usd: worst,
                    remaining_usd: remaining,
                });
            }
            req.max_tokens = req.max_tokens.min(afford as u32);
            // A thinking budget must stay under the output limit.
            if let Some(tb) = req.thinking_budget {
                if tb >= req.max_tokens {
                    req.thinking_budget =
                        Some(req.max_tokens / 2).filter(|b| *b >= MIN_OUTPUT_TOKENS);
                }
            }
        }
        let (rec, out) = match self.provider.complete(req) {
            Ok(r) => {
                let cost = profile.cost_usd(&r.usage);
                let calls = r
                    .blocks
                    .iter()
                    .filter(|b| matches!(b, crate::ir::Block::ToolCall { .. }))
                    .count() as u32;
                let rec = UsageRecord::from_usage(
                    req.model,
                    &r.usage,
                    r.request_bytes,
                    r.latency_ms,
                    calls,
                    cost,
                );
                (rec, Ok((r, cost)))
            }
            Err(e) => (
                UsageRecord::from_usage(req.model, &Usage::default(), 0, 0, 0, 0.0),
                Err(GateError::Provider(e)),
            ),
        };
        self.ledger
            .record(UsageRecord {
                purpose: purpose.map(str::to_string),
                ..rec
            })
            .map_err(GateError::Io)?;
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Settlement rows move cost, never tokens, and re-settling the same
    /// total (a second drain, a resume) records nothing.
    #[test]
    fn settlement_is_idempotent_and_token_free() {
        let dir = std::env::temp_dir().join(format!("overseer-settle-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        let mut l = Ledger::create(&path).unwrap();
        l.record(rec(100, 0, 0, 10)).unwrap();
        let own = l.total_cost_usd;
        assert_eq!(l.settle("task-1", "claude-haiku-4-5", 0.20).unwrap(), 0.20);
        assert_eq!(l.settle("task-1", "claude-haiku-4-5", 0.20).unwrap(), 0.0);
        // A resumed subagent's ledger grew to 0.30: only the delta lands.
        assert!((l.settle("task-1", "claude-haiku-4-5", 0.30).unwrap() - 0.10).abs() < 1e-9);
        assert_eq!(l.calls, 1, "settlements are not calls");
        assert_eq!(l.cache_stats().fresh_input, 100);
        let reopened = Ledger::open(&path).unwrap();
        assert!((reopened.total_cost_usd - (own + 0.30)).abs() < 1e-9);
        assert!((reopened.subagent_cost_usd - 0.30).abs() < 1e-9);
        assert_eq!(reopened.calls, 1);
        let mut reopened = reopened;
        assert_eq!(
            reopened.settle("task-1", "m", 0.30).unwrap(),
            0.0,
            "replayed settle"
        );
        let s = Ledger::summarize(&Ledger::read_all(&path));
        assert_eq!(s.calls, 1);
        assert!((s.subagent_cost_usd - 0.30).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `purpose` tag (memory v3 review calls) round-trips and is
    /// absent on pre-v3 rows — old ledgers replay unchanged.
    #[test]
    fn purpose_tag_round_trips_and_old_rows_have_none() {
        let dir = std::env::temp_dir().join(format!("overseer-purpose-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("ledger.jsonl");
        let mut l = Ledger::create(&path).unwrap();
        l.record(UsageRecord {
            purpose: Some("memory_review".into()),
            ..rec(10, 0, 0, 5)
        })
        .unwrap();
        l.record(rec(20, 0, 0, 5)).unwrap();
        drop(l);
        let rows = Ledger::read_all(&path);
        assert_eq!(rows[0].purpose.as_deref(), Some("memory_review"));
        assert_eq!(rows[1].purpose, None);
        // A pre-v3 line (no `purpose` key at all) deserializes.
        let old = r#"{"ts_ms":1,"model":"m","fresh_input":1,"cache_write":0,"cache_read":0,"output":1,"reasoning":0,"request_bytes":0,"latency_ms":0,"tool_calls":0,"cost_usd":0.01,"cache_hit_rate":0.0}"#;
        let r: UsageRecord = serde_json::from_str(old).unwrap();
        assert_eq!(r.purpose, None);
        // And None never serializes — the column stays out of old rows.
        assert!(!serde_json::to_string(&r).unwrap().contains("purpose"));
        let _ = std::fs::remove_dir_all(&dir);
    }

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
            subagent: None,
            purpose: None,
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
            subagent: None,
            purpose: None,
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
            subagent: None,
            purpose: None,
        }];
        let s2 = Ledger::summarize(&high);
        assert!(!s2.cache_alert, "0.95 hit rate must not alert");
    }
}
