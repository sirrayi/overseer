// Ported from Synara (MIT, (c) 2026 T3 Tools Inc., (c) 2026 Emanuele Di Pietro),
// apps/server/src/providerUsageSnapshot.ts — local archive token totals for
// Claude transcripts (~/.claude/projects/<dir>/*.jsonl) and Codex session
// rollouts (~/.codex/sessions/**/*.jsonl). Same de-dup keys, same per-file
// and per-line caps. Pure filesystem: no network, no CLI spawns.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::connector::{Connector, ConnectorInfo, Ctx, Discovered};
use crate::snapshot::{
    as_f64, as_non_negative, as_str, format_compact, Metric, Provenance, ProvenanceKind, Snapshot,
    SourceId, Status, UsageLimit, UsageLine,
};
use crate::time::unix_seconds_to_ms;

const ONE_DAY_MS: i64 = 24 * 60 * 60 * 1000;
const LOOKBACK_7D_MS: i64 = 7 * ONE_DAY_MS;
const LOOKBACK_30D_MS: i64 = 30 * ONE_DAY_MS;
const MAX_RECENT_USAGE_FILES: usize = 2_000;
const MAX_LINE_BYTES: usize = 1024 * 1024;
/// Codex rollouts are read tail-first — a session's last token_count
/// event carries cumulative totals, so the head of a huge file is dead
/// weight. 8 MiB of tail is generous for that last event.
const CODEX_TAIL_BYTES: u64 = 8 * 1024 * 1024;
const MAX_WALK_DEPTH: usize = 6;

fn provenance() -> Provenance {
    Provenance {
        kind: ProvenanceKind::LocalArchive,
        endpoint: None,
        documented: false,
    }
}

fn file_discovery(path: &Path) -> Discovered {
    Discovered {
        kind: "file",
        location: path.display().to_string(),
        contains: "session archive (read-only scan)",
        present: path.is_dir(),
    }
}

fn mtime_ms(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Recursively collect *.jsonl under `root` (depth-bounded), newest-first,
/// capped — Synara's `listRecentFiles`. (Synara enumerated YYYY/MM/DD dirs
/// by local date; walking the bounded tree covers the same set regardless
/// of which clock wrote the directory names.)
fn recent_jsonl(root: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack: Vec<(PathBuf, usize)> = vec![(root.to_path_buf(), 0)];
    while let Some((dir, depth)) = stack.pop() {
        if depth > MAX_WALK_DEPTH {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_dir() {
                stack.push((path, depth + 1));
            } else if ft.is_file() && path.extension().is_some_and(|e| e == "jsonl") {
                files.push(path);
            }
        }
    }
    files.sort_by_key(|p| std::cmp::Reverse(mtime_ms(p)));
    files.truncate(MAX_RECENT_USAGE_FILES);
    files
}

fn usage_lines(t24: (f64, usize), t7: (f64, usize), t30: (f64, usize)) -> Vec<UsageLine> {
    let mut lines = Vec::new();
    for (label, (tokens, sessions)) in [("24h", t24), ("7d", t7), ("30d", t30)] {
        let value = format!("{} tokens", format_compact(tokens));
        let value = if sessions > 0 {
            format!(
                "{value} across {sessions} recent session{}",
                if sessions == 1 { "" } else { "s" }
            )
        } else {
            value
        };
        lines.push(UsageLine {
            label: label.into(),
            value,
        });
    }
    lines
}

fn window_metrics(snap: &mut Snapshot, t24: (f64, f64), t7: (f64, f64), t30: (f64, f64)) {
    for (label, (tokens, sessions)) in [("24h", t24), ("7d", t7), ("30d", t30)] {
        snap.metrics.push(Metric {
            name: format!("tokens_{label}"),
            value: tokens,
            unit: Some("tokens".into()),
        });
        snap.metrics.push(Metric {
            name: format!("sessions_{label}"),
            value: sessions,
            unit: Some("sessions".into()),
        });
    }
}

// ---------------------------------------------------------------------------
// Claude transcripts
// ---------------------------------------------------------------------------

struct ClaudeSample {
    session_id: String,
    timestamp_ms: i64,
    total_tokens: f64,
}

fn read_claude_total_tokens(usage: &Value) -> f64 {
    let input = as_non_negative(&usage["input_tokens"]).unwrap_or(0.0)
        + as_non_negative(&usage["cache_creation_input_tokens"]).unwrap_or(0.0)
        + as_non_negative(&usage["cache_read_input_tokens"]).unwrap_or(0.0);
    let output = as_non_negative(&usage["output_tokens"]).unwrap_or(0.0);
    as_non_negative(&usage["total_tokens"]).unwrap_or(input + output)
}

fn read_claude_samples(path: &Path) -> Vec<ClaudeSample> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let mut samples = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut reader = BufReader::new(file);
    let mut line_index = 0usize;
    loop {
        let mut buf = Vec::new();
        // Cap each record at 1 MiB; an oversized line is skipped entirely
        // (Synara's skippingOversizedLine).
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        line_index += 1;
        // read_until already consumed through the newline — an oversized
        // record is skipped whole (Synara's skippingOversizedLine).
        if buf.len() > MAX_LINE_BYTES {
            continue;
        }
        let Ok(text) = std::str::from_utf8(&buf) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let Ok(record) = serde_json::from_str::<Value>(text) else {
            continue;
        };
        let fallback = format!("{}:{line_index}", path.display());
        let ts = record["timestamp"]
            .as_str()
            .and_then(crate::time::parse_rfc3339_ms);
        let Some(timestamp_ms) = ts else { continue };

        // assistant turns: message.usage
        if record["type"].as_str() == Some("assistant") {
            let usage = &record["message"]["usage"];
            let total = read_claude_total_tokens(usage);
            if usage.is_object() && total > 0.0 {
                let session_id = as_str(&record["sessionId"])
                    .map(str::to_string)
                    .unwrap_or_else(|| fallback.clone());
                let key = format!(
                    "{session_id}:assistant:{}",
                    as_str(&record["requestId"])
                        .or_else(|| as_str(&record["message"]["id"]))
                        .or_else(|| as_str(&record["uuid"]))
                        .unwrap_or(&fallback)
                );
                if seen.insert(key) {
                    samples.push(ClaudeSample {
                        session_id,
                        timestamp_ms,
                        total_tokens: total,
                    });
                }
            }
        }

        // toolUseResult.usage (subagent/tool rows)
        let tur = &record["toolUseResult"];
        if tur.is_object() {
            let usage = &tur["usage"];
            let total = read_claude_total_tokens(usage);
            if usage.is_object() && total > 0.0 {
                let session_id = as_str(&record["sessionId"])
                    .map(str::to_string)
                    .unwrap_or_else(|| fallback.clone());
                let key = format!(
                    "{session_id}:tool-result:{}",
                    as_str(&record["uuid"])
                        .or_else(|| as_str(&tur["agentId"]))
                        .or_else(|| as_str(&record["requestId"]))
                        .unwrap_or(&fallback)
                );
                if seen.insert(key) {
                    samples.push(ClaudeSample {
                        session_id,
                        timestamp_ms,
                        total_tokens: total,
                    });
                }
            }
        }
    }
    samples
}

pub struct ClaudeArchiveConnector;

impl Connector for ClaudeArchiveConnector {
    fn id(&self) -> SourceId {
        SourceId::from("claude-local")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "claude-local",
            name: "Claude project transcripts",
            needs_network: false,
        }
    }

    fn discover(&self, ctx: &Ctx) -> Vec<Discovered> {
        vec![file_discovery(&claude_projects_root(ctx))]
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let root = claude_projects_root(ctx);
        let files = recent_jsonl(&root);
        if files.is_empty() {
            return Snapshot::unavailable(
                self.id(),
                ctx.now_ms,
                provenance(),
                "no Claude transcript archive under ~/.claude/projects",
            );
        }
        let mut samples = Vec::new();
        for f in &files {
            samples.extend(read_claude_samples(f));
        }
        if samples.is_empty() {
            return Snapshot::unavailable(
                self.id(),
                ctx.now_ms,
                provenance(),
                "Claude transcripts contain no usage records",
            );
        }
        let c24 = ctx.now_ms - ONE_DAY_MS;
        let c7 = ctx.now_ms - LOOKBACK_7D_MS;
        let c30 = ctx.now_ms - LOOKBACK_30D_MS;
        let sum = |cutoff: i64| -> (f64, HashSet<String>) {
            let mut tokens = 0.0;
            let mut sessions = HashSet::new();
            for s in samples.iter().filter(|s| s.timestamp_ms >= cutoff) {
                tokens += s.total_tokens;
                sessions.insert(s.session_id.clone());
            }
            (tokens, sessions)
        };
        let (t24, s24) = sum(c24);
        let (t7, s7) = sum(c7);
        let (t30, s30) = sum(c30);
        let mut snap = Snapshot::new(self.id(), ctx.now_ms, Status::Ok, provenance());
        snap.usage_lines = usage_lines((t24, s24.len()), (t7, s7.len()), (t30, s30.len()));
        window_metrics(
            &mut snap,
            (t24, s24.len() as f64),
            (t7, s7.len() as f64),
            (t30, s30.len() as f64),
        );
        snap
    }
}

fn claude_projects_root(ctx: &Ctx) -> PathBuf {
    let config = ctx
        .env("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.home(&[".claude"]));
    config.join("projects")
}

// ---------------------------------------------------------------------------
// Codex session rollouts
// ---------------------------------------------------------------------------

struct CodexSummary {
    timestamp_ms: i64,
    total_tokens: f64,
    limits: Vec<UsageLimit>,
}

fn read_codex_total_tokens(payload: &Value) -> f64 {
    let info = &payload["info"];
    let total_usage = if info["total_token_usage"].is_object() {
        &info["total_token_usage"]
    } else if info["totalTokenUsage"].is_object() {
        &info["totalTokenUsage"]
    } else if info["total"].is_object() {
        &info["total"]
    } else if payload["total_token_usage"].is_object() {
        &payload["total_token_usage"]
    } else if payload["totalTokenUsage"].is_object() {
        &payload["totalTokenUsage"]
    } else if payload["total"].is_object() {
        &payload["total"]
    } else {
        &Value::Null
    };
    as_non_negative(&total_usage["total_tokens"])
        .or_else(|| as_non_negative(&total_usage["totalTokens"]))
        .or_else(|| as_non_negative(&info["total_tokens"]))
        .or_else(|| as_non_negative(&info["totalTokens"]))
        .or_else(|| as_non_negative(&payload["total_tokens"]))
        .or_else(|| as_non_negative(&payload["totalTokens"]))
        .unwrap_or(0.0)
}

fn codex_limits(value: &Value) -> Vec<UsageLimit> {
    let mut out = Vec::new();
    for (label, key) in [("5h", "primary"), ("Weekly", "secondary")] {
        let src = &value[key];
        if !src.is_object() {
            continue;
        }
        let used_percent =
            as_non_negative(&src["used_percent"]).or_else(|| as_non_negative(&src["usedPercent"]));
        let minutes = as_non_negative(&src["window_minutes"])
            .or_else(|| as_non_negative(&src["windowMinutes"]))
            .map(|m| m as u32);
        let resets_at_ms = src["resets_at"]
            .as_str()
            .and_then(crate::time::parse_rfc3339_ms)
            .or_else(|| {
                src["resetsAt"]
                    .as_str()
                    .and_then(crate::time::parse_rfc3339_ms)
            })
            .or_else(|| as_str(&src["next_reset_at"]).and_then(crate::time::parse_rfc3339_ms))
            .or_else(|| as_str(&src["nextResetAt"]).and_then(crate::time::parse_rfc3339_ms))
            .or_else(|| unix_seconds_to_ms(as_f64(&src["resets_at"])));
        if used_percent.is_none() && minutes.is_none() && resets_at_ms.is_none() {
            continue;
        }
        out.push(UsageLimit {
            name: label.to_string(),
            used_percent,
            window_minutes: minutes,
            resets_at_ms,
            ..Default::default()
        });
    }
    out
}

fn parse_codex_summary_line(line: &str) -> Option<CodexSummary> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    let record: Value = serde_json::from_str(line).ok()?;
    if record["type"].as_str() != Some("event_msg") {
        return None;
    }
    let payload = &record["payload"];
    if payload["type"].as_str() != Some("token_count") {
        return None;
    }
    let timestamp_ms = record["timestamp"]
        .as_str()
        .and_then(crate::time::parse_rfc3339_ms)
        .or_else(|| {
            payload["timestamp"]
                .as_str()
                .and_then(crate::time::parse_rfc3339_ms)
        })?;
    Some(CodexSummary {
        timestamp_ms,
        total_tokens: read_codex_total_tokens(payload),
        limits: {
            let v = codex_limits(&payload["rate_limits"]);
            if v.is_empty() {
                codex_limits(&payload["rateLimits"])
            } else {
                v
            }
        },
    })
}

/// Read a session file tail-first (bounded) and return the last
/// `token_count` event — Synara's backwards chunked scan.
fn read_codex_summary(path: &Path) -> Option<CodexSummary> {
    let mut file = std::fs::File::open(path).ok()?;
    let size = file.metadata().ok()?.len();
    let start = size.saturating_sub(CODEX_TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.take(CODEX_TAIL_BYTES).read_to_end(&mut tail).ok()?;
    // Skip a partial first line when we seeked mid-line.
    let text = String::from_utf8_lossy(&tail);
    let mut lines: Vec<&str> = text.split('\n').collect();
    if start > 0 {
        lines.remove(0);
    }
    for line in lines.iter().rev() {
        if line.len() > MAX_LINE_BYTES {
            continue;
        }
        if let Some(summary) = parse_codex_summary_line(line) {
            return Some(summary);
        }
    }
    None
}

pub struct CodexArchiveConnector;

impl Connector for CodexArchiveConnector {
    fn id(&self) -> SourceId {
        SourceId::from("codex-local")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "codex-local",
            name: "Codex session archive",
            needs_network: false,
        }
    }

    fn discover(&self, ctx: &Ctx) -> Vec<Discovered> {
        vec![file_discovery(&codex_sessions_root(ctx))]
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let root = codex_sessions_root(ctx);
        let files = recent_jsonl(&root);
        if files.is_empty() {
            return Snapshot::unavailable(
                self.id(),
                ctx.now_ms,
                provenance(),
                "no Codex session archive under ~/.codex/sessions",
            );
        }
        let mut summaries = Vec::new();
        for f in &files {
            if let Some(s) = read_codex_summary(f) {
                summaries.push(s);
            }
        }
        if summaries.is_empty() {
            return Snapshot::unavailable(
                self.id(),
                ctx.now_ms,
                provenance(),
                "Codex sessions contain no token_count events",
            );
        }
        let c24 = ctx.now_ms - ONE_DAY_MS;
        let c7 = ctx.now_ms - LOOKBACK_7D_MS;
        let c30 = ctx.now_ms - LOOKBACK_30D_MS;
        let sum = |cutoff: i64| -> (f64, usize) {
            let recent: Vec<&CodexSummary> = summaries
                .iter()
                .filter(|s| s.timestamp_ms >= cutoff)
                .collect();
            (recent.iter().map(|s| s.total_tokens).sum(), recent.len())
        };
        let (t24, s24) = sum(c24);
        let (t7, s7) = sum(c7);
        let (t30, s30) = sum(c30);
        let mut snap = Snapshot::new(self.id(), ctx.now_ms, Status::Ok, provenance());
        // Latest summary carries the most recent rate-limit windows.
        if let Some(latest) = summaries.iter().max_by_key(|s| s.timestamp_ms) {
            snap.limits = latest.limits.clone();
        }
        snap.usage_lines = usage_lines((t24, s24), (t7, s7), (t30, s30));
        window_metrics(
            &mut snap,
            (t24, s24 as f64),
            (t7, s7 as f64),
            (t30, s30 as f64),
        );
        snap
    }
}

fn codex_sessions_root(ctx: &Ctx) -> PathBuf {
    ctx.env("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.home(&[".codex"]))
        .join("sessions")
}
