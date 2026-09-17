//! Intervention gate (playbook §2.3): push only when
//! E[benefit] − E[cost] > θ. "Silence is a first-class action" — the
//! default for anything that can't justify the interruption is the
//! inbox, and quiet hours demote pushes to inbox items.

use crate::config::GateConfig;
use crate::event::now_ms;
use crate::triage::Triage;

/// Where a surfaced event lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Journal only — observed, not surfaced (digest fodder).
    Silent,
    /// Async review item — the default tier.
    Inbox,
    /// Immediate notification — rare; benefit beat cost + θ.
    Push,
}

/// EV check: benefit − cost > theta. `always_push` classes and a zero-cost
/// window bypass; quiet hours raise the effective cost to max.
pub fn route(gate: &GateConfig, class: &str, triage: &Triage, quiet: bool) -> Route {
    if gate.always_push.iter().any(|c| c == class) {
        return Route::Push;
    }
    let cost = if quiet { 100 } else { gate.cost } as i16;
    if triage.benefit as i16 - cost > gate.theta {
        Route::Push
    } else if triage.benefit == 0 {
        Route::Silent
    } else {
        Route::Inbox
    }
}

/// Parse "HH:MM" → minutes since midnight.
fn hhmm(s: &str) -> Option<u16> {
    let (h, m) = s.split_once(':')?;
    Some(h.parse::<u16>().ok()? * 60 + m.parse::<u16>().ok()?)
}

/// Local-time minutes since midnight without a chrono dep: epoch →
/// localtime via `date +%H:%M` is too slow per tick, so compute UTC +
/// the system TZ offset once per call using libc-free math — seconds in
/// day mod 86400, adjusted by `TZ` being ignored: we use std::time only,
/// and treat "local" as UTC unless `OVERSEER_TZ_OFFSET_MIN` is set.
/// (Keeps the daemon dependency-free; accurate quiet hours land with the
/// desktop frontend that can read real clock/DND state.)
pub fn local_minutes(now_ms_val: u64) -> u16 {
    let offset: i64 = std::env::var("OVERSEER_TZ_OFFSET_MIN")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    let secs = (now_ms_val / 1000) as i64 + offset * 60;
    (((secs % 86_400) + 86_400) % 86_400 / 60) as u16
}

/// Is now inside quiet hours? Range may wrap midnight ("22:00"–"08:00").
pub fn in_quiet_hours(gate: &GateConfig) -> bool {
    let Some(q) = &gate.quiet_hours else {
        return false;
    };
    let (Some(start), Some(end)) = (hhmm(&q.start), hhmm(&q.end)) else {
        return false;
    };
    let now = local_minutes(now_ms());
    if start <= end {
        now >= start && now < end
    } else {
        now >= start || now < end
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{GateConfig, QuietHours, TriageDecision};
    use crate::triage::Triage;

    fn gate(theta: i16, cost: u8) -> GateConfig {
        GateConfig {
            theta,
            cost,
            quiet_hours: None,
            always_push: Vec::new(),
        }
    }

    fn triage(benefit: u8) -> Triage {
        Triage {
            decision: TriageDecision::Notify,
            benefit,
            act_prompt: None,
        }
    }

    fn quiet_gate(start: &str, end: &str) -> GateConfig {
        GateConfig {
            theta: 30,
            cost: 40,
            quiet_hours: Some(QuietHours {
                start: start.to_string(),
                end: end.to_string(),
            }),
            always_push: Vec::new(),
        }
    }

    /// Run `f` with `OVERSEER_TZ_OFFSET_MIN` set/removed, restoring the
    /// previous value afterwards. The lock serializes every test that
    /// observes local time so concurrent tests can't see a moved clock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_tz_offset<T>(offset: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().expect("env lock");
        let prev = std::env::var("OVERSEER_TZ_OFFSET_MIN").ok();
        match offset {
            Some(v) => std::env::set_var("OVERSEER_TZ_OFFSET_MIN", v),
            None => std::env::remove_var("OVERSEER_TZ_OFFSET_MIN"),
        }
        let out = f();
        match prev {
            Some(v) => std::env::set_var("OVERSEER_TZ_OFFSET_MIN", v),
            None => std::env::remove_var("OVERSEER_TZ_OFFSET_MIN"),
        }
        out
    }

    #[test]
    fn always_push_bypasses_gate_even_when_quiet_and_benefitless() {
        let mut g = gate(30, 40);
        g.always_push.push("security.alert".to_string());
        // Benefit 0 + quiet would otherwise be Silent; the bypass still fires.
        assert_eq!(route(&g, "security.alert", &triage(0), true), Route::Push);
        // Non-listed classes still go through the gate.
        assert_eq!(route(&g, "git.dirty", &triage(80), false), Route::Push);
        assert_eq!(route(&g, "git.dirty", &triage(0), false), Route::Silent);
    }

    #[test]
    fn benefit_minus_cost_above_theta_pushes() {
        // 80 − 40 = 40 > 30.
        assert_eq!(route(&gate(30, 40), "x", &triage(80), false), Route::Push);
    }

    #[test]
    fn equal_margin_does_not_push_falls_to_inbox() {
        // 70 − 40 = 30 == θ: strict `>` fails, but benefit > 0 → Inbox.
        assert_eq!(route(&gate(30, 40), "x", &triage(70), false), Route::Inbox);
    }

    #[test]
    fn zero_benefit_is_silent() {
        assert_eq!(route(&gate(30, 40), "x", &triage(0), false), Route::Silent);
    }

    #[test]
    fn quiet_hours_demote_push_to_inbox() {
        // 80 − 100 = −20 ≯ 30, but benefit > 0 → Inbox instead of Push.
        assert_eq!(route(&gate(30, 40), "x", &triage(80), true), Route::Inbox);
    }

    #[test]
    fn in_quiet_hours_wrap_midnight() {
        with_tz_offset(None, || {
            let g = quiet_gate("22:00", "08:00");
            let now = local_minutes(now_ms());
            let expected = !(8 * 60..22 * 60).contains(&now);
            assert_eq!(in_quiet_hours(&g), expected);
        });
    }

    #[test]
    fn in_quiet_hours_non_wrapping_range() {
        with_tz_offset(None, || {
            let g = quiet_gate("09:00", "17:00");
            let now = local_minutes(now_ms());
            let expected = (9 * 60..17 * 60).contains(&now);
            assert_eq!(in_quiet_hours(&g), expected);
        });
    }

    #[test]
    fn in_quiet_hours_malformed_or_missing_is_false() {
        with_tz_offset(None, || {
            assert!(!in_quiet_hours(&gate(30, 40)));
            assert!(!in_quiet_hours(&quiet_gate("bogus", "08:00")));
            assert!(!in_quiet_hours(&quiet_gate("22:00", "bogus")));
            assert!(!in_quiet_hours(&quiet_gate("", "")));
        });
    }

    #[test]
    fn local_minutes_respects_tz_offset() {
        with_tz_offset(None, || {
            assert_eq!(local_minutes(0), 0);
            assert_eq!(local_minutes(3_600_000), 60);
        });
        with_tz_offset(Some("60"), || {
            assert_eq!(local_minutes(0), 60);
        });
        with_tz_offset(Some("-30"), || {
            // −30 min wraps to the previous day: 1440 − 30.
            assert_eq!(local_minutes(0), 1410);
        });
        with_tz_offset(Some("1500"), || {
            // Offsets larger than a day wrap mod 1440: 1500 − 1440 = 60.
            assert_eq!(local_minutes(0), 60);
        });
    }
}
