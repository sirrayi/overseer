//! Trigger sources — cron-ish intervals, file watchers, heartbeats.
//!
//! All sources are pull-based against the daemon tick: no OS callbacks,
//! no threads per trigger, nothing that can wedge. A watcher polls mtimes
//! (cheap `read_dir`, no `notify` dep) and emits at most one event per
//! changed file per scan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::channels::{self, sentinel, telegram, webhook};
use crate::config::{TelegramSpec, TriggerSpec, WebhookSpec};
use crate::event::{now_ms, TriggerEvent};

/// Runtime state for one configured trigger.
pub enum Trigger {
    Interval {
        id: String,
        every_ms: u64,
        next_ms: u64,
        body: String,
        class: String,
    },
    Watch {
        id: String,
        dir: PathBuf,
        name_contains: String,
        body: String,
        class: String,
        /// path → last seen mtime (ms); seeds on first scan so existing
        /// files don't storm the inbox at daemon start.
        mtimes: HashMap<PathBuf, u64>,
        primed: bool,
    },
    Heartbeat {
        id: String,
        every_ms: u64,
        next_ms: u64,
    },
    /// Wall-clock cron (P7-4): fires at most once per matching minute.
    Cron {
        id: String,
        expr: CronExpr,
        body: String,
        class: String,
        /// Epoch minute (ms/60_000) already fired — the once-per-minute
        /// latch. Epoch-based so tomorrow's same wall-clock minute is a
        /// different minute (a minute-of-day latch would silence it).
        last_minute: Option<u64>,
    },
    /// Webhook spool (P7-4): verified inbound records become untrusted
    /// events; refusals become `channel.rejected` events (never a silent
    /// drop) and the record is moved aside so it is not retried forever.
    Webhook {
        id: String,
        dir: PathBuf,
        spec: WebhookSpec,
        limiter: webhook::RateLimiter,
    },
    /// Telegram long-poll (P7-4). `client` is `None` while the token
    /// variable is unset — the daemon journals that at startup and the
    /// trigger stays quiet rather than blocking a tick on the network.
    Telegram {
        id: String,
        spec: TelegramSpec,
        client: Option<telegram::Telegram>,
        allow_senders: Vec<String>,
        limiter: webhook::RateLimiter,
        offset: Option<i64>,
    },
}

impl Trigger {
    pub fn from_spec(spec: &TriggerSpec) -> Self {
        let now = now_ms();
        match spec {
            TriggerSpec::Interval {
                id,
                every_s,
                body,
                class,
            } => Trigger::Interval {
                id: id.clone(),
                every_ms: every_s * 1000,
                next_ms: now + every_s * 1000,
                body: body.clone(),
                class: if class.is_empty() {
                    "interval".to_string()
                } else {
                    class.clone()
                },
            },
            TriggerSpec::Watch {
                id,
                dir,
                name_contains,
                body,
                class,
            } => Trigger::Watch {
                id: id.clone(),
                dir: dir.clone(),
                name_contains: name_contains.clone(),
                body: body.clone(),
                class: if class.is_empty() {
                    "fs.change".to_string()
                } else {
                    class.clone()
                },
                mtimes: HashMap::new(),
                primed: false,
            },
            TriggerSpec::Heartbeat { id, every_s } => Trigger::Heartbeat {
                id: id.clone(),
                every_ms: every_s * 1000,
                next_ms: now + every_s * 1000,
            },
            TriggerSpec::Cron {
                id,
                expr,
                body,
                class,
            } => Trigger::Cron {
                id: id.clone(),
                // An unparseable expression never fires — the daemon
                // journals `cron_invalid` at startup via `cron_error`.
                expr: CronExpr::parse(expr).unwrap_or_else(|_| CronExpr::never()),
                body: body.clone(),
                class: if class.is_empty() {
                    "cron".to_string()
                } else {
                    class.clone()
                },
                last_minute: None,
            },
            TriggerSpec::Webhook(spec) => Trigger::Webhook {
                id: spec.id.clone(),
                dir: spec.dir.clone(),
                spec: spec.clone(),
                limiter: webhook::RateLimiter::new(spec.rate_per_min),
            },
            TriggerSpec::Telegram(spec) => Trigger::Telegram {
                id: spec.id.clone(),
                spec: spec.clone(),
                client: telegram::Telegram::from_env(&spec.token_env)
                    .ok()
                    .map(|c| c.with_base(spec.base.clone())),
                allow_senders: spec.allow_senders.clone(),
                limiter: webhook::RateLimiter::new(spec.rate_per_min),
                offset: None,
            },
        }
    }

    /// The reason this trigger cannot run, if any (unset token, bad cron).
    /// The daemon journals it once at startup so a misconfiguration is
    /// visible instead of just quiet.
    pub fn unconfigured_reason(&self) -> Option<String> {
        match self {
            Trigger::Telegram { spec, client, .. } if client.is_none() => Some(format!(
                "telegram trigger '{}': {} is unset — no bot token",
                spec.id, spec.token_env
            )),
            Trigger::Cron { id, expr, .. } => {
                expr.error().map(|e| format!("cron trigger '{id}': {e}"))
            }
            _ => None,
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Trigger::Interval { id, .. }
            | Trigger::Watch { id, .. }
            | Trigger::Heartbeat { id, .. }
            | Trigger::Cron { id, .. }
            | Trigger::Webhook { id, .. }
            | Trigger::Telegram { id, .. } => id,
        }
    }

    /// Poll the trigger; returns any events due this tick.
    pub fn poll(&mut self) -> Vec<TriggerEvent> {
        self.poll_at(now_ms())
    }

    /// Polling with an injected clock — the fake-clock seam (P7-4/P7-6).
    pub fn poll_at(&mut self, now: u64) -> Vec<TriggerEvent> {
        match self {
            Trigger::Interval {
                id,
                every_ms,
                next_ms,
                body,
                class,
            } => {
                if now < *next_ms {
                    return Vec::new();
                }
                // Catch-up: skip missed periods rather than storming.
                *next_ms = now + *every_ms;
                vec![TriggerEvent::new(id.as_str(), class.clone(), body.clone())]
            }
            Trigger::Heartbeat {
                id,
                every_ms,
                next_ms,
            } => {
                if now < *next_ms {
                    return Vec::new();
                }
                *next_ms = now + *every_ms;
                vec![TriggerEvent::new(id.as_str(), "heartbeat", "check-in")]
            }
            Trigger::Watch {
                id,
                dir,
                name_contains,
                body,
                class,
                mtimes,
                primed,
            } => poll_watch(id, dir, name_contains, body, class, mtimes, primed),
            Trigger::Cron {
                id,
                expr,
                body,
                class,
                last_minute,
            } => {
                let Some(t) = CivilTime::from_epoch_ms(now) else {
                    return Vec::new();
                };
                if expr.matches(&t) {
                    let epoch_minute = now / 60_000;
                    if *last_minute == Some(epoch_minute) {
                        return Vec::new(); // already fired this minute
                    }
                    *last_minute = Some(epoch_minute);
                    vec![TriggerEvent::new(id.as_str(), class.clone(), body.clone())]
                } else {
                    Vec::new()
                }
            }
            Trigger::Webhook {
                id: _,
                dir,
                spec,
                limiter,
            } => {
                let secret = sentinel::load_token(&spec.secret_env);
                poll_webhook(dir, spec, secret.as_deref(), limiter, now)
            }
            Trigger::Telegram {
                id: _,
                spec: _,
                client,
                allow_senders,
                limiter,
                offset,
            } => {
                let Some(client) = client.as_ref() else {
                    return Vec::new();
                };
                let polled = match client.poll(*offset) {
                    Ok(p) => p,
                    Err(e) => return vec![channels::rejection_event("telegram", &e)],
                };
                // Advance the offset first: a re-poll must never re-deliver
                // the same update, even if a message below is refused.
                if polled.next_offset.is_some() {
                    *offset = polled.next_offset;
                }
                let mut events = Vec::new();
                for inbound in polled.messages {
                    if !webhook::sender_allowed(allow_senders, &inbound.sender) {
                        events.push(channels::rejection_event(
                            "telegram",
                            &format!("sender '{}' is not on the allowlist", inbound.sender),
                        ));
                        continue;
                    }
                    if !limiter.admit_at(&inbound.sender, now) {
                        events.push(channels::rejection_event(
                            "telegram",
                            &format!("rate limit exceeded for '{}'", inbound.sender),
                        ));
                        continue;
                    }
                    events.push(channels::event_for(&inbound));
                }
                events
            }
        }
    }
}

/// Drain the webhook spool: each `*.json` record is verified and ingested,
/// then moved aside (`.done` / `.rejected`) so nothing is retried forever.
fn poll_webhook(
    dir: &Path,
    spec: &WebhookSpec,
    secret: Option<&str>,
    limiter: &mut webhook::RateLimiter,
    now: u64,
) -> Vec<TriggerEvent> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("json"))
        .collect();
    files.sort();
    let mut events = Vec::new();
    for path in files {
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let outcome = webhook::parse_request(&text)
            .and_then(|req| webhook::ingest(spec, secret, &req, limiter, now));
        match outcome {
            Ok(ev) => events.push(ev),
            Err(reason) => events.push(channels::rejection_event("webhook", &reason)),
        }
        let done = if events.last().is_some_and(|e| e.class == "channel.rejected") {
            path.with_extension("rejected")
        } else {
            path.with_extension("done")
        };
        let _ = std::fs::rename(&path, done);
    }
    events
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn poll_watch(
    id: &str,
    dir: &Path,
    name_contains: &str,
    body: &str,
    class: &str,
    mtimes: &mut HashMap<PathBuf, u64>,
    primed: &mut bool,
) -> Vec<TriggerEvent> {
    let mut out = Vec::new();
    let mut live = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !name_contains.is_empty() && !name.contains(name_contains) {
                continue;
            }
            let m = mtime_ms(&path);
            live.push(path.clone());
            match mtimes.get(&path) {
                Some(prev) if *prev == m => {}
                Some(_) => {
                    mtimes.insert(path.clone(), m);
                    out.push(TriggerEvent::new(
                        id,
                        class.to_string(),
                        format!("{body}: {}", name),
                    ));
                }
                None => {
                    mtimes.insert(path.clone(), m);
                    if *primed {
                        out.push(TriggerEvent::new(
                            id,
                            class.to_string(),
                            format!("{body}: {}", name),
                        ));
                    }
                }
            }
        }
    }
    // Forget deleted files so a re-created file fires as new.
    mtimes.retain(|p, _| live.contains(p));
    *primed = true;
    out
}

// ── Cron (P7-4) ────────────────────────────────────────────────────────

/// One cron field: the set of values it accepts. `*` = every value in
/// range; `a,b`, `a-b`, `*/n`, and `a-b/n` compose on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    min: u32,
    max: u32,
    set: Vec<bool>,
    /// Set when the field covers its whole range — documented, not special.
    any: bool,
}

impl Field {
    fn any(min: u32, max: u32) -> Self {
        Field {
            min,
            max,
            set: vec![true; (max - min + 1) as usize],
            any: true,
        }
    }

    fn never(min: u32, max: u32) -> Self {
        Field {
            min,
            max,
            set: vec![false; (max - min + 1) as usize],
            any: false,
        }
    }

    fn contains(&self, v: u32) -> bool {
        if v < self.min || v > self.max {
            return false;
        }
        self.set[(v - self.min) as usize]
    }

    fn advance(&mut self, v: u32) {
        if v >= self.min && v <= self.max {
            self.set[(v - self.min) as usize] = true;
        }
    }

    /// Parse one field. Ranges are inclusive; steps skip within the range.
    fn parse(text: &str, min: u32, max: u32, names: Option<&[&str]>) -> Result<Self, String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("empty cron field".into());
        }
        let mut field = if text == "*" || text == "*/1" {
            Field::any(min, max)
        } else {
            Field::never(min, max)
        };
        for part in text.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(format!("empty cron term in '{text}'"));
            }
            let (spec, step) = match part.split_once('/') {
                Some((s, n)) => {
                    let n: u32 = n
                        .parse()
                        .map_err(|_| format!("bad cron step '{n}' (want a number)"))?;
                    if n == 0 {
                        return Err("cron step of 0 fires nothing".into());
                    }
                    (s.trim(), n)
                }
                None => (part, 1),
            };
            let val = |t: &str| -> Result<u32, String> {
                if let Some(names) = names {
                    if let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(t)) {
                        return Ok(min + i as u32);
                    }
                }
                t.parse::<u32>()
                    .map_err(|_| format!("bad cron value '{t}' (want {min}-{max})"))
            };
            let (lo, hi) = match spec {
                "*" => (min, max),
                _ => match spec.split_once('-') {
                    Some((a, b)) => {
                        let (a, b) = (val(a.trim())?, val(b.trim())?);
                        if a > b {
                            return Err(format!("cron range '{spec}' runs backwards"));
                        }
                        (a, b)
                    }
                    None => {
                        let v = val(spec)?;
                        (v, v)
                    }
                },
            };
            if lo < min || hi > max {
                return Err(format!("cron value '{spec}' outside {min}-{max}"));
            }
            let mut v = lo;
            while v <= hi {
                field.advance(v);
                v += step;
            }
            field.any = false;
        }
        if field.set.iter().all(|b| !*b) {
            return Err(format!("cron field '{text}' matches nothing"));
        }
        Ok(field)
    }
}

/// A parsed 5-field cron expression (`minute hour dom month dow`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CronExpr {
    minute: Field,
    hour: Field,
    dom: Field,
    month: Field,
    dow: Field,
    /// Parse failure, kept so the daemon can journal it (a bad expression
    /// never fires — fail-closed, never "every minute").
    error: Option<String>,
}

impl CronExpr {
    pub fn parse(expr: &str) -> Result<Self, String> {
        let parts: Vec<&str> = expr.split_whitespace().collect();
        if parts.len() != 5 {
            return Err(format!(
                "cron expression needs 5 fields (minute hour dom month dow), got {}",
                parts.len()
            ));
        }
        Ok(CronExpr {
            minute: Field::parse(parts[0], 0, 59, None)?,
            hour: Field::parse(parts[1], 0, 23, None)?,
            dom: Field::parse(parts[2], 1, 31, None)?,
            month: Field::parse(parts[3], 1, 12, None)?,
            dow: Field::parse(
                parts[4],
                0,
                6,
                Some(&["sun", "mon", "tue", "wed", "thu", "fri", "sat"]),
            )?,
            error: None,
        })
    }

    /// An expression that can never fire — used when the configured text is
    /// invalid (the reason travels via [`CronExpr::error`]).
    pub fn never() -> Self {
        CronExpr {
            minute: Field::never(0, 59),
            hour: Field::never(0, 23),
            dom: Field::never(1, 31),
            month: Field::never(1, 12),
            dow: Field::never(0, 6),
            error: Some("invalid cron expression".to_string()),
        }
    }

    /// The parse error, if this expression came from invalid text.
    pub fn error(&self) -> Option<String> {
        self.error.clone()
    }

    pub fn matches_minute(&self, minute: u32) -> bool {
        self.minute.contains(minute)
    }

    /// Whether this expression fires at `t`. Day-of-month and day-of-week
    /// follow cron's OR rule when both are restricted (Vixie cron), which
    /// is what operators expect from `0 9 1 * mon`.
    pub fn matches(&self, t: &CivilTime) -> bool {
        if !self.minute.contains(t.minute)
            || !self.hour.contains(t.hour)
            || !self.month.contains(t.month)
        {
            return false;
        }
        let dom_any = (1..=31).all(|d| self.dom.contains(d));
        let dow_any = (0..=6).all(|d| self.dow.contains(d));
        match (dom_any, dow_any) {
            (true, true) => true,
            (true, false) => self.dow.contains(t.dow),
            (false, true) => self.dom.contains(t.day),
            (false, false) => self.dom.contains(t.day) || self.dow.contains(t.dow),
        }
    }
}

/// A wall-clock instant, resolved without a date library. Local time is
/// UTC plus `OVERSEER_TZ_OFFSET_MIN` (same seam the intervention gate
/// uses) — the daemon stays dependency-free and testable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilTime {
    pub minute: u32,
    pub hour: u32,
    pub day: u32,
    pub month: u32,
    /// 0 = Sunday.
    pub dow: u32,
}

impl CivilTime {
    /// Resolve an epoch-ms timestamp to local civil time. `None` before
    /// 1970 (nothing schedulable lives there).
    pub fn from_epoch_ms(ms: u64) -> Option<Self> {
        let offset_min: i64 = std::env::var("OVERSEER_TZ_OFFSET_MIN")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let secs = (ms / 1000) as i64 + offset_min * 60;
        if secs < 0 {
            return None;
        }
        let days = secs / 86_400;
        let secs_of_day = secs % 86_400;
        let (_, month, day) = civil_from_days(days);
        Some(CivilTime {
            minute: ((secs_of_day / 60) % 60) as u32,
            hour: (secs_of_day / 3600) as u32,
            day,
            month,
            // 1970-01-01 was a Thursday (4).
            dow: ((days + 4) % 7) as u32,
        })
    }
}

/// Days-since-epoch → (year, month, day). Howard Hinnant's `civil_from_days`
/// (public-domain *algorithm*, arithmetic only — no code copied).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::tmpdir;

    fn interval_spec(id: &str, every_s: u64) -> TriggerSpec {
        TriggerSpec::Interval {
            id: id.to_string(),
            every_s,
            body: "body".to_string(),
            class: String::new(),
        }
    }

    #[test]
    fn interval_zero_period_fires_immediately() {
        let mut trigger = Trigger::from_spec(&interval_spec("int-zero", 0));
        assert_eq!(trigger.id(), "int-zero");
        let events = trigger.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "int-zero");
        assert_eq!(events[0].class, "interval");
        assert_eq!(events[0].payload, "body");
    }

    #[test]
    fn interval_respects_period() {
        let mut trigger = Trigger::from_spec(&interval_spec("int-hourly", 3600));
        assert!(trigger.poll().is_empty());
        let mut overdue = Trigger::Interval {
            id: "int-overdue".to_string(),
            every_ms: 3_600_000,
            next_ms: now_ms().saturating_sub(1),
            body: "body".to_string(),
            class: "interval".to_string(),
        };
        assert_eq!(overdue.poll().len(), 1);
        assert!(
            overdue.poll().is_empty(),
            "catch-up must advance past the fired period"
        );
    }

    #[test]
    fn heartbeat_fires_on_schedule() {
        let mut trigger = Trigger::from_spec(&TriggerSpec::Heartbeat {
            id: "hb-zero".to_string(),
            every_s: 0,
        });
        assert_eq!(trigger.id(), "hb-zero");
        let events = trigger.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "hb-zero");
        assert_eq!(events[0].class, "heartbeat");
        let mut quiet = Trigger::from_spec(&TriggerSpec::Heartbeat {
            id: "hb-hourly".to_string(),
            every_s: 3600,
        });
        assert!(quiet.poll().is_empty());
    }

    #[test]
    fn watch_primes_then_fires_on_new_and_changed_file() {
        let dir = tmpdir("trigger-watch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut trigger = Trigger::from_spec(&TriggerSpec::Watch {
            id: "watch".to_string(),
            dir: dir.clone(),
            name_contains: String::new(),
            body: "changed".to_string(),
            class: String::new(),
        });
        assert_eq!(trigger.id(), "watch");
        assert!(trigger.poll().is_empty(), "first scan only primes");
        let file = dir.join("note.txt");
        std::fs::write(&file, "v1").unwrap();
        let events = trigger.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "watch");
        assert!(trigger.poll().is_empty(), "steady state is quiet");
        let before = mtime_ms(&file);
        let mut n = 0u32;
        while mtime_ms(&file) == before && n < 100_000 {
            n += 1;
            std::fs::write(&file, format!("v2-{n}")).unwrap();
        }
        assert_ne!(
            mtime_ms(&file),
            before,
            "content rewrite must advance mtime"
        );
        assert_eq!(trigger.poll().len(), 1);
    }

    // ── P7-4: cron, webhook spool, telegram ────────────────────────────

    #[test]
    fn cron_parses_fields_and_rejects_nonsense() {
        // Every shorthand the config may carry parses to a value set.
        let e = CronExpr::parse("*/15 9-17 1,15 * mon-fri").expect("valid expression");
        for m in [0, 15, 30, 45] {
            assert!(e.matches_minute(m), "minute {m} must match */15");
        }
        for m in [1, 14, 16, 59] {
            assert!(!e.matches_minute(m), "minute {m} must not match */15");
        }
        assert!(CronExpr::parse("* * * * *").is_ok());
        assert!(CronExpr::parse("0 0 1 1 0").is_ok());
        assert!(
            CronExpr::parse("0 9 * * SUN").is_ok(),
            "names are case-insensitive"
        );
        // Wrong arity / values / ranges / emptiness all fail closed.
        for bad in [
            "",
            "* * * *",
            "* * * * * *",
            "60 * * * *",
            "* 24 * * *",
            "* * 0 * *",
            "* * * 13 *",
            "* * * * 7",
            "5-1 * * * *",
            "*/0 * * * *",
            "abc * * * *",
        ] {
            assert!(CronExpr::parse(bad).is_err(), "{bad:?} must be rejected");
        }
        // A never-expression matches nothing (fail-closed, not "always").
        let never = CronExpr::never();
        assert!(never.error().is_some());
        let t = CivilTime::from_epoch_ms(1_760_000_000_000).unwrap();
        assert!(!never.matches(&t));
    }

    #[test]
    fn cron_fires_once_per_matching_minute() {
        // 2026-09-18T09:30:00Z; the test derives the expression from the
        // resolved civil time so it holds under any TZ offset.
        let at = 1_789_723_800_000u64;
        let t = CivilTime::from_epoch_ms(at).expect("civil time");
        assert!(t.minute < 60 && t.hour < 24 && (1..=31).contains(&t.day));
        assert!(t.month <= 12 && t.dow <= 6);
        // A fixed-offset clock: one day later is the same wall-clock minute.
        let next_day = CivilTime::from_epoch_ms(at + 86_400_000).unwrap();
        assert_eq!((next_day.hour, next_day.minute), (t.hour, t.minute));
        assert_eq!((t.dow + 1) % 7, next_day.dow);
        assert_eq!(
            CivilTime::from_epoch_ms(0).unwrap().dow,
            4,
            "1970-01-01 was a Thursday (UTC)"
        );

        let expr = CronExpr::parse(&format!("{} {} * * *", t.minute, t.hour)).unwrap();
        assert!(expr.matches(&t));
        assert!(
            !CronExpr::parse(&format!("{} {} * * *", (t.minute + 1) % 60, t.hour))
                .unwrap()
                .matches(&t)
        );

        // The runtime latch: one event per matching minute, no repeats.
        let spec = TriggerSpec::Cron {
            id: "nightly".into(),
            expr: format!("{} {} * * *", t.minute, t.hour),
            body: "review the diff".into(),
            class: String::new(),
        };
        let mut trigger = Trigger::from_spec(&spec);
        let events = trigger.poll_at(at);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].class, "cron");
        assert_eq!(events[0].source, "nightly");
        assert_eq!(events[0].payload, "review the diff");
        assert!(trigger.poll_at(at + 1_000).is_empty(), "same minute");
        assert!(trigger.poll_at(at + 59_999).is_empty(), "same minute");
        assert!(trigger.poll_at(at + 60_000).is_empty(), "minute after");
        assert_eq!(
            trigger.poll_at(at + 86_400_000).len(),
            1,
            "next day, same minute"
        );

        // Invalid expressions never fire and report why.
        let mut bad = Trigger::from_spec(&TriggerSpec::Cron {
            id: "bad".into(),
            expr: "not a cron".into(),
            body: "x".into(),
            class: String::new(),
        });
        assert!(bad.poll_at(at).is_empty());
        assert!(bad.unconfigured_reason().is_some());
        // Day-of-month OR day-of-week (Vixie rule).
        let e = CronExpr::parse("0 0 1 * mon").unwrap();
        assert!(e.matches(&CivilTime {
            minute: 0,
            hour: 0,
            day: 1,
            month: 9,
            dow: 2
        }));
        assert!(e.matches(&CivilTime {
            minute: 0,
            hour: 0,
            day: 7,
            month: 9,
            dow: 1
        }));
        assert!(!e.matches(&CivilTime {
            minute: 0,
            hour: 0,
            day: 7,
            month: 9,
            dow: 2
        }));
    }

    #[test]
    fn webhook_spool_ingests_verified_records_and_moves_them_aside() {
        let dir = tmpdir("webhook-spool");
        let spool = dir.join("webhook");
        std::fs::create_dir_all(&spool).unwrap();
        let spec = WebhookSpec {
            id: "wf".into(),
            secret_env: "OVERSEER_TEST_SPOOL_SECRET".into(),
            allow_senders: vec!["u1".into()],
            rate_per_min: 5,
            dir: spool.clone(),
            body: String::new(),
            class: String::new(),
        };
        // Two records: one correctly signed, one tampered.
        let good = r#"{"sender":"u1","thread":"t9","text":"/queue ship the fix"}"#;
        std::fs::write(
            spool.join("a.json"),
            serde_json::json!({
                "signature": webhook::hex(&webhook::hmac_sha256(b"k", good.as_bytes())),
                "body": good,
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(
            spool.join("b.json"),
            serde_json::json!({"signature": "sha256=00", "body": good}).to_string(),
        )
        .unwrap();
        // The secret arrives as a parameter (env read is the caller's job).
        let mut limiter = webhook::RateLimiter::new(5);
        let events = poll_webhook(&spool, &spec, Some("k"), &mut limiter, 1_000);
        assert_eq!(events.len(), 2);
        let ok = events
            .iter()
            .find(|e| e.class != "channel.rejected")
            .unwrap();
        assert!(ok.untrusted_source);
        assert_eq!(ok.class, "msg.inbound.queue");
        assert_eq!(ok.source, "webhook:u1");
        let rejected = events
            .iter()
            .find(|e| e.class == "channel.rejected")
            .unwrap();
        assert!(rejected.untrusted_source);
        assert!(
            rejected.payload.contains("signature"),
            "got {}",
            rejected.payload
        );
        // Records are moved aside: the next tick sees nothing (no replay).
        assert!(poll_webhook(&spool, &spec, Some("k"), &mut limiter, 2_000).is_empty());
        assert!(spool.join("a.done").exists());
        assert!(spool.join("b.rejected").exists());
        // Unset secret → every record is refused, never silently accepted.
        let mut limiter = webhook::RateLimiter::new(5);
        std::fs::write(
            spool.join("c.json"),
            serde_json::json!({"signature": "sha256=00", "body": good}).to_string(),
        )
        .unwrap();
        let spec_unset = WebhookSpec {
            secret_env: "OVERSEER_SPOOL_SECRET_UNSET_31a".into(),
            ..spec.clone()
        };
        let events = poll_webhook(&spool, &spec_unset, None, &mut limiter, 3_000);
        assert_eq!(events.len(), 1);
        assert!(
            events[0].payload.contains("unset"),
            "got {}",
            events[0].payload
        );
    }

    #[test]
    fn telegram_trigger_is_quiet_when_unconfigured_and_journals_why() {
        let mut trigger = Trigger::from_spec(&TriggerSpec::Telegram(TelegramSpec {
            id: "tg".into(),
            token_env: "OVERSEER_TELEGRAM_TOKEN_UNSET_77b".into(),
            allow_senders: vec!["77".into()],
            rate_per_min: 5,
            base: "http://127.0.0.1:1".into(),
        }));
        assert!(trigger
            .unconfigured_reason()
            .is_some_and(|r| r.contains("OVERSEER_TELEGRAM_TOKEN_UNSET_77b")));
        assert!(
            trigger.poll_at(1_000).is_empty(),
            "no token, no network call"
        );
        // Configured but unreachable: the failure is an event, not a panic
        // — and it carries no token.
        let mut live = Trigger::from_spec(&TriggerSpec::Telegram(TelegramSpec {
            id: "tg".into(),
            token_env: "PATH".into(), // a set variable: "TOKEN" must never be logged
            allow_senders: vec!["77".into()],
            rate_per_min: 5,
            base: "http://127.0.0.1:1".into(),
        }));
        assert!(live.unconfigured_reason().is_none());
        let events = live.poll_at(1_000);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].class, "channel.rejected");
        assert!(events[0].untrusted_source);
    }
}
