//! Audit: `OVERSEER_TZ_OFFSET_MIN` handling. Own test binary because the
//! offset is process environment; a lock keeps the two tests serial.

use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Mutex;

use overseer_gateway::config::TriggerSpec;
use overseer_gateway::gate;
use overseer_gateway::trigger::Trigger;
use serde_json::json;

static ENV: Mutex<()> = Mutex::new(());
const VAR: &str = "OVERSEER_TZ_OFFSET_MIN";

fn with_offset<T>(v: Option<&str>, f: impl FnOnce() -> T) -> T {
    let _g = ENV.lock().unwrap_or_else(|e| e.into_inner());
    match v {
        Some(v) => std::env::set_var(VAR, v),
        None => std::env::remove_var(VAR),
    }
    let out = f();
    std::env::remove_var(VAR);
    out
}

#[test]
fn ordinary_and_garbage_offsets() {
    // 1970-01-01T00:00Z.
    assert_eq!(with_offset(None, || gate::local_minutes(0)), 0);
    assert_eq!(with_offset(Some("840"), || gate::local_minutes(0)), 840); // UTC+14
    assert_eq!(with_offset(Some("-720"), || gate::local_minutes(0)), 720); // UTC-12, prior day
    assert_eq!(with_offset(Some(" 330 "), || gate::local_minutes(0)), 330);
    assert_eq!(with_offset(Some("abc"), || gate::local_minutes(0)), 0);
    assert_eq!(with_offset(Some("5.5"), || gate::local_minutes(0)), 0);
    // Day wrap at the far end of u64 milliseconds stays in range.
    let m = with_offset(Some("-840"), || gate::local_minutes(u64::MAX));
    assert!(m < 1440);
}

/// An absurd offset must fall back like an unparseable one, not panic the
/// gate (every routed event) or the cron clock (every tick).
#[test]
#[ignore = "audit: gate-tz-overflow"]
fn extreme_offset_does_not_panic_gate_or_cron() {
    let max = i64::MAX.to_string();
    let (gate_r, cron_r) = with_offset(Some(&max), || {
        let g = catch_unwind(|| gate::local_minutes(1_700_000_000_000));
        let spec: TriggerSpec = serde_json::from_value(json!({
            "kind": "cron", "id": "c", "expr": "* * * * *", "body": "b"
        }))
        .unwrap();
        let mut t = Trigger::from_spec(&spec);
        let c = catch_unwind(AssertUnwindSafe(|| t.poll_at(1_700_000_000_000).len()));
        (g, c)
    });
    assert!(
        gate_r.is_ok() && cron_r.is_ok(),
        "OVERSEER_TZ_OFFSET_MIN=i64::MAX panicked: gate={} cron={}",
        gate_r.is_err(),
        cron_r.is_err()
    );
}
