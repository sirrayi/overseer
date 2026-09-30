//! Budget composition: every subagent spends out of its parent's cap.
//!
//! The parent's ledger is the record — settled subagent spend lands there
//! as settlement rows. What the ledger cannot see yet (a subagent still
//! running, or finished but not yet settled) is held here as a
//! reservation of its cap, so concurrent spawns can never overspend.
//! Shared (`Arc`) between the agent and its background threads, which
//! draw escalation caps from it.

use std::collections::BTreeMap;
use std::sync::Mutex;

use super::route::TaskMode;
use crate::profile::Tier;

/// Default per-spawn caps (USD). A spawn takes the smaller of its
/// requested `max_cost_usd` (else this default) and what the parent has
/// left; consult is one bounded call, hence its own figure.
pub const LIGHT_CAP_USD: f64 = 0.25;
pub const STANDARD_CAP_USD: f64 = 1.00;
pub const HEAVY_CAP_USD: f64 = 2.00;
pub const CONSULT_CAP_USD: f64 = 0.50;
/// Below this a spawn is refused — it could not finish one request.
pub const MIN_CAP_USD: f64 = 0.01;

pub fn default_cap(mode: TaskMode, tier: Tier) -> f64 {
    match (mode, tier) {
        (TaskMode::Consult, _) => CONSULT_CAP_USD,
        (_, Tier::Light) => LIGHT_CAP_USD,
        (_, Tier::Standard) => STANDARD_CAP_USD,
        (_, Tier::Heavy) => HEAVY_CAP_USD,
    }
}

#[derive(Debug)]
pub struct SpendAccount {
    max_usd: f64,
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Parent ledger total (own calls + settled subagents), last synced.
    recorded_usd: f64,
    /// Task id → cap held for spend the ledger does not show yet.
    reserved: BTreeMap<String, f64>,
}

impl Inner {
    fn remaining(&self, max_usd: f64) -> f64 {
        max_usd - self.recorded_usd - self.reserved.values().fold(0.0, |a, b| a + b)
    }
}

impl SpendAccount {
    pub fn new(max_usd: f64, recorded_usd: f64) -> Self {
        SpendAccount {
            max_usd,
            inner: Mutex::new(Inner {
                recorded_usd,
                reserved: BTreeMap::new(),
            }),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn reserved_usd(&self) -> f64 {
        self.lock().reserved.values().fold(0.0, |a, b| a + b)
    }

    pub fn remaining_usd(&self) -> f64 {
        self.lock().remaining(self.max_usd)
    }

    /// Atomically reserve `min(want, remaining)` for `id` — added to any
    /// reservation it already holds. `Err(remaining)` when that is below
    /// `floor`; nothing is reserved then.
    pub fn grant(&self, id: &str, want: f64, floor: f64) -> Result<f64, f64> {
        let mut g = self.lock();
        let remaining = g.remaining(self.max_usd);
        let cap = want.min(remaining);
        if cap < floor {
            return Err(remaining.max(0.0));
        }
        *g.reserved.entry(id.to_string()).or_default() += cap;
        Ok(cap)
    }

    /// Re-hold the cap of a task that is still running but has no
    /// reservation here (a rebuilt agent: a session switch or rewind while
    /// it runs). Clamped to what remains; when that is below `floor` the
    /// full cap is recorded anyway, so the cost check sees the overshoot
    /// instead of an account that looks free. Returns what is now held.
    pub fn restore(&self, id: &str, cap: f64, floor: f64) -> f64 {
        let mut g = self.lock();
        if let Some(held) = g.reserved.get(id) {
            return *held;
        }
        let remaining = g.remaining(self.max_usd);
        let held = if cap.min(remaining) >= floor {
            cap.min(remaining)
        } else {
            cap
        };
        g.reserved.insert(id.to_string(), held);
        held
    }

    /// `id` finished but cannot be settled normally: its cap is released
    /// down to `spent`, which stays held until the ledger shows it.
    pub fn shrink(&self, id: &str, spent: f64) {
        let mut g = self.lock();
        if let Some(held) = g.reserved.get_mut(id) {
            *held = held.min(spent.max(0.0));
        }
    }

    /// Drop `id`'s reservation without settling (the spawn never ran).
    pub fn release(&self, id: &str) {
        self.lock().reserved.remove(id);
    }

    /// Mirror the parent ledger total after a record it made itself.
    pub fn sync(&self, recorded_usd: f64) {
        self.lock().recorded_usd = recorded_usd;
    }

    /// `id`'s spend is now in the parent ledger (total `recorded_usd`):
    /// its reservation goes in the same critical section.
    pub fn settle(&self, id: &str, recorded_usd: f64) {
        let mut g = self.lock();
        g.reserved.remove(id);
        g.recorded_usd = recorded_usd;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restore_clamps_then_records_the_overshoot() {
        let acct = SpendAccount::new(1.00, 0.20);
        let near = |a: f64, b: f64| (a - b).abs() < 1e-9;
        assert!(near(acct.restore("task-1", 0.50, MIN_CAP_USD), 0.50));
        // Idempotent: a held reservation is left as is.
        assert!(near(acct.restore("task-1", 0.90, MIN_CAP_USD), 0.50));
        // Partly covered: clamped to the $0.30 left.
        assert!(near(acct.restore("task-2", 1.00, MIN_CAP_USD), 0.30));
        // Nothing left: the full cap still counts against the parent.
        assert!(near(acct.restore("task-3", 0.25, MIN_CAP_USD), 0.25));
        assert!(near(acct.reserved_usd(), 1.05));
        assert!(0.20 + acct.reserved_usd() > 1.00, "overshoot visible");
    }

    /// Worked example of the arithmetic (run with --nocapture to see it).
    #[test]
    fn caps_clamp_to_what_the_parent_has_left() {
        let acct = SpendAccount::new(1.00, 0.30);
        let show = |step: &str| {
            println!(
                "{step:<44} remaining ${:.2} (reserved ${:.2})",
                acct.remaining_usd(),
                acct.reserved_usd()
            )
        };
        show("parent max $1.00, own spend $0.30");
        let light = default_cap(TaskMode::Read, Tier::Light);
        let near = |r: Result<f64, f64>, want: f64| (r.unwrap() - want).abs() < 1e-9;
        assert!(near(acct.grant("task-1", light, MIN_CAP_USD), 0.25));
        show("task-1 read/light: min($0.25, $0.70)");
        assert!(near(
            acct.grant("task-2", STANDARD_CAP_USD, MIN_CAP_USD),
            0.45
        ));
        show("task-2 write/standard: min($1.00, $0.45)");
        assert!(acct.grant("task-3", light, MIN_CAP_USD).unwrap_err() < 1e-9);
        show("task-3 refused: $0.00 < $0.01");
        // task-1 settles at its actual $0.10: the ledger now shows it.
        acct.settle("task-1", 0.40);
        show("task-1 settled at $0.10 (ledger $0.40)");
        let r = acct.grant("task-4", light, MIN_CAP_USD).unwrap();
        assert!((r - 0.15).abs() < 1e-9, "{r}");
        show("task-4 read/light: min($0.25, $0.15)");
        acct.release("task-4");
        assert!((acct.remaining_usd() - 0.15).abs() < 1e-9);
    }

    #[test]
    fn default_caps_by_mode_and_tier() {
        assert_eq!(default_cap(TaskMode::Read, Tier::Light), 0.25);
        assert_eq!(default_cap(TaskMode::Write, Tier::Standard), 1.00);
        assert_eq!(default_cap(TaskMode::Verify, Tier::Heavy), 2.00);
        assert_eq!(default_cap(TaskMode::Consult, Tier::Heavy), 0.50);
    }
}
