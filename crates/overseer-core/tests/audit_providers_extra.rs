//! Pricing audit: unknown models never price at $0; only the flat-rate
//! subscription rows in `profile.rs` do.

use overseer_core::ir::Usage;
use overseer_core::profile::{known, lookup};

fn usage() -> Usage {
    Usage {
        fresh_input: 10_000,
        cache_write: 1_000,
        cache_read: 5_000,
        output: 2_000,
        reasoning: 500,
    }
}

#[test]
fn unknown_model_prices_above_zero() {
    for m in [
        "",
        "no-such-model",
        "vendor/unknown-9",
        "gpt-9",
        "muse-next",
    ] {
        assert!(!known(m), "{m:?} unexpectedly tabled");
        let c = lookup(m).cost_usd(&usage());
        assert!(c > 0.0, "{m:?} priced at {c}");
    }
}

#[test]
fn subscription_models_price_at_zero() {
    for m in ["deepseek-v4.1-flash", "muse-spark-1.3-contributor"] {
        assert!(known(m), "{m:?} not tabled");
        assert_eq!(lookup(m).cost_usd(&usage()), 0.0, "{m:?}");
    }
}
