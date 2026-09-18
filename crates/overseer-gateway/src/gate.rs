//! Intervention gate (playbook §2.3): push only when
//! E[benefit] − E[cost] > θ. "Silence is a first-class action" — the
//! default for anything that can't justify the interruption is the
//! inbox, and quiet hours demote pushes to inbox items.

use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

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

// ── P7-6 desktop hooks: attention state → interruption cost ────────────

/// The desktop's attention state, pushed by the frontend each tick
/// (`desktop_signal`). Everything is a fact the frontend observed; nothing
/// here is inferred by the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct FocusState {
    /// A window is focused and the user is present.
    pub focused: bool,
    /// Do-not-disturb is on.
    pub dnd: bool,
    /// A calendar event is in progress.
    pub calendar_busy: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_app: Option<String>,
    /// Seconds since the last input event, when the frontend can tell.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_s: Option<u64>,
}

impl FocusState {
    /// A quiet, present desktop: the shape a frontend sends when it has
    /// nothing special to report.
    pub fn idle_present() -> Self {
        FocusState::default()
    }

    /// The user has stepped away — a natural breakpoint.
    pub fn away(&self) -> bool {
        self.idle_s.is_some_and(|s| s >= AWAY_AFTER_S)
    }

    /// An interruption lands free at a breakpoint: present-but-unfocused,
    /// or away long enough that nothing is displaced.
    pub fn at_breakpoint(&self) -> bool {
        self.away() || !self.focused
    }
}

/// Input silence that counts as "away" (five minutes, the usual screen-lock
/// heuristic).
pub const AWAY_AFTER_S: u64 = 300;

/// Relief applied while the user is away: nothing is being displaced.
pub const AWAY_RELIEF: u8 = 20;

/// The EV-of-interruption cost for an attention state (Horvitz §2.3):
/// DND — and a busy calendar, when configured as quiet — costs the maximum,
/// exactly like quiet hours; a focused user costs more than the configured
/// baseline; an away user costs less. Never above 100, never under 0.
///
/// An away state overrides `focused`: a focused window on a machine nobody
/// has touched for [`AWAY_AFTER_S`] is stale information, and the relief
/// applies.
pub fn cost_for(state: &FocusState, cfg: &GateConfig) -> u8 {
    if state.dnd || (state.calendar_busy && cfg.calendar_as_quiet) {
        return 100;
    }
    if state.away() {
        return cfg.cost.saturating_sub(AWAY_RELIEF);
    }
    let base = u16::from(cfg.cost);
    let boosted = if state.focused {
        base + u16::from(cfg.focus_boost)
    } else {
        base
    };
    boosted.min(100) as u8
}

/// `route` with a live attention state. Quiet hours still force the maximum
/// cost; the desktop state replaces the static `cost`. A Push decided while
/// the user is *not* at a breakpoint is deferred to the inbox (the
/// breakpoint-defer rule) unless the class is on `always_push`.
pub fn route_desktop(
    gate: &GateConfig,
    class: &str,
    triage: &Triage,
    quiet: bool,
    focus: Option<&FocusState>,
) -> Route {
    if gate.always_push.iter().any(|c| c == class) {
        return Route::Push;
    }
    let Some(state) = focus else {
        return route(gate, class, triage, quiet);
    };
    let cost = if quiet { 100 } else { cost_for(state, gate) } as i16;
    if triage.benefit as i16 - cost > gate.theta {
        if state.at_breakpoint() {
            Route::Push
        } else {
            // Worth saying, wrong moment: it waits for the breakpoint.
            Route::Inbox
        }
    } else if triage.benefit == 0 {
        Route::Silent
    } else {
        Route::Inbox
    }
}

/// Clock seam: the desktop readers take time as data, so a test can move it.
pub trait Clock {
    fn now_ms(&self) -> u64;
}

/// The real clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        now_ms()
    }
}

/// A clock the test owns.
#[derive(Debug, Default)]
pub struct FakeClock {
    now: AtomicU64,
}

impl FakeClock {
    pub fn new(start_ms: u64) -> Self {
        FakeClock {
            now: AtomicU64::new(start_ms),
        }
    }

    pub fn set(&self, ms: u64) {
        self.now.store(ms, Ordering::SeqCst);
    }

    pub fn advance(&self, ms: u64) {
        self.now.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for FakeClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

/// Where the desktop facts come from. The real reader lives in the desktop
/// frontend (per-OS APIs); the daemon only ever sees what a frontend pushes
/// or a reader returns through this trait — no OS call in the daemon.
pub trait FocusSource {
    fn state(&self, now_ms: u64) -> FocusState;
}

/// Calendar / DND reader driven by a provider trait + a clock, so both a
/// real desktop provider and a test can supply the same facts.
pub trait CalendarSource {
    /// True while a calendar event covers `now_ms`.
    fn busy_at(&self, now_ms: u64) -> bool;
}

/// The last state a frontend pushed — what the daemon actually uses.
#[derive(Debug, Default)]
pub struct PushedFocus {
    state: std::sync::Mutex<Option<(u64, FocusState)>>,
}

impl FocusSource for PushedFocus {
    /// The daemon's view: whatever the frontend last pushed (a quiet desktop
    /// when nothing has been pushed yet). `now_ms` is accepted for the seam —
    /// a provider that ages its own facts uses it.
    fn state(&self, _now_ms: u64) -> FocusState {
        self.latest().unwrap_or_default()
    }
}

impl PushedFocus {
    pub fn push(&self, at_ms: u64, state: FocusState) {
        if let Ok(mut slot) = self.state.lock() {
            *slot = Some((at_ms, state));
        }
    }

    /// The most recent state, if any (a stale push is still the best
    /// information available — the frontend owns freshness).
    pub fn latest(&self) -> Option<FocusState> {
        self.state
            .lock()
            .ok()
            .and_then(|s| s.clone())
            .map(|(_, st)| st)
    }
}

/// A calendar reader backed by a list of busy windows. This is the fake a
/// test drives; a real provider implements [`CalendarSource`] the same way.
#[derive(Debug, Default, Clone)]
pub struct BusyWindows {
    windows: Vec<(u64, u64)>,
}

impl BusyWindows {
    pub fn new(windows: &[(u64, u64)]) -> Self {
        BusyWindows {
            windows: windows.to_vec(),
        }
    }
}

impl CalendarSource for BusyWindows {
    fn busy_at(&self, now_ms: u64) -> bool {
        self.windows
            .iter()
            .any(|(start, end)| now_ms >= *start && now_ms < *end)
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
            ..GateConfig::default()
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
            ..GateConfig::default()
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

    // ── P7-6 tests ─────────────────────────────────────────────────────

    #[test]
    fn cost_table_tracks_attention_state() {
        let cfg = GateConfig {
            theta: 30,
            cost: 40,
            ..GateConfig::default()
        };
        assert_eq!(cfg.focus_boost, 20);
        assert!(cfg.calendar_as_quiet);
        // Baseline: present, unfocused.
        let base = FocusState::idle_present();
        assert_eq!(cost_for(&base, &cfg), 40);
        // Focused costs more, but never past 100.
        let focused = FocusState {
            focused: true,
            ..base.clone()
        };
        assert_eq!(cost_for(&focused, &cfg), 60);
        let loud = GateConfig {
            cost: 95,
            focus_boost: 50,
            ..cfg.clone()
        };
        assert_eq!(cost_for(&focused, &loud), 100);
        // DND and a busy calendar cost the maximum, like quiet hours.
        let dnd = FocusState {
            dnd: true,
            ..base.clone()
        };
        assert_eq!(cost_for(&dnd, &cfg), 100);
        let busy = FocusState {
            calendar_busy: true,
            ..base.clone()
        };
        assert_eq!(cost_for(&busy, &cfg), 100);
        // …unless the operator opts out of calendar-as-quiet.
        let calm = GateConfig {
            calendar_as_quiet: false,
            ..cfg.clone()
        };
        assert_eq!(cost_for(&busy, &calm), 40);
        // Away is cheaper (nothing is displaced), floored at zero.
        let away = FocusState {
            focused: true,
            idle_s: Some(AWAY_AFTER_S),
            ..base.clone()
        };
        assert_eq!(cost_for(&away, &cfg), 20, "away relief applies");
        let tight = GateConfig {
            cost: 5,
            focus_boost: 0,
            ..cfg.clone()
        };
        assert_eq!(cost_for(&away, &tight), 0, "no negative cost");
        // Breakpoint reasoning: away or unfocused.
        assert!(away.at_breakpoint() && away.away());
        assert!(base.at_breakpoint(), "unfocused desktop is a breakpoint");
        assert!(!focused.at_breakpoint());
    }

    #[test]
    fn quiet_calendar_and_breakpoint_defer_route_the_event() {
        let cfg = GateConfig {
            theta: 30,
            cost: 40,
            ..GateConfig::default()
        };
        let hot = triage(90); // benefit 90: pushes at day cost 40
        assert_eq!(route(&cfg, "ci.failed", &hot, false), Route::Push);
        // A focused user: the same event is worth saying but deferred to
        // the next breakpoint, so it lands in the inbox.
        let focused = FocusState {
            focused: true,
            ..FocusState::default()
        };
        assert_eq!(
            cost_for(&focused, &cfg),
            60,
            "focused raises the cost above the theta margin"
        );
        assert_eq!(
            route_desktop(&cfg, "ci.failed", &hot, false, Some(&focused)),
            Route::Inbox,
            "not worth interrupting a focused user"
        );
        // A busy calendar is as expensive as quiet hours: even a high
        // benefit defers.
        let busy = FocusState {
            focused: true,
            calendar_busy: true,
            idle_s: Some(30),
            ..FocusState::default()
        };
        assert_eq!(
            route_desktop(
                &cfg,
                "security.alert.mild",
                &triage(100),
                false,
                Some(&busy)
            ),
            Route::Inbox
        );
        // Unfocused desktop → push as usual.
        let unfocused = FocusState {
            focused: false,
            ..FocusState::default()
        };
        assert_eq!(
            route_desktop(&cfg, "ci.failed", &hot, false, Some(&unfocused)),
            Route::Push
        );
        // always_push still escapes, focused or not (that is its contract).
        let mut bypass = cfg.clone();
        bypass.always_push.push("security.alert".into());
        assert_eq!(
            route_desktop(&bypass, "security.alert", &triage(5), false, Some(&focused)),
            Route::Push
        );
        // Quiet hours keep their meaning with a state present too.
        assert_eq!(
            route_desktop(&cfg, "ci.failed", &triage(0), true, Some(&unfocused)),
            Route::Silent
        );
        // No state → the pre-P7 behaviour, byte for byte.
        for (benefit, quiet) in [(90u8, false), (10, false), (0, false), (90, true)] {
            let t = triage(benefit);
            assert_eq!(
                route_desktop(&cfg, "ci.failed", &t, quiet, None),
                route(&cfg, "ci.failed", &t, quiet)
            );
        }
    }

    #[test]
    fn fake_clock_and_calendar_reader_drive_the_state() {
        // The fake-clock seam: the reader takes time as data.
        let clock = FakeClock::new(1_000);
        let windows = BusyWindows::new(&[(900, 2_000), (5_000, 5_600)]);
        assert!(!windows.busy_at(500));
        assert!(windows.busy_at(clock.now_ms()));
        clock.advance(2_000);
        assert_eq!(clock.now_ms(), 3_000);
        assert!(!windows.busy_at(clock.now_ms()), "meeting ended");
        clock.set(5_100);
        assert!(windows.busy_at(clock.now_ms()), "second meeting");
        let real = SystemClock;
        assert!(real.now_ms() > 1_600_000_000_000, "system clock is real");

        // The provider traits compose: a calendar reader owns `busy`, the
        // frontend's push owns the rest.
        let pushed = FocusState {
            focused: true,
            active_app: Some("iTerm".into()),
            idle_s: Some(3),
            calendar_busy: windows.busy_at(5_100),
            ..FocusState::default()
        };
        assert!(pushed.calendar_busy && pushed.focused);
        assert_eq!(pushed.active_app.as_deref(), Some("iTerm"));

        // PushedFocus is the daemon's FocusSource: what the frontend last
        // said, or a quiet desktop when nothing was pushed yet.
        let slot = PushedFocus::default();
        assert_eq!(slot.state(0), FocusState::default());
        assert!(slot.latest().is_none());
        slot.push(10, FocusState::default());
        slot.push(20, pushed.clone());
        assert_eq!(slot.latest(), Some(pushed.clone()));
        assert_eq!(slot.state(1_000), pushed);
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
