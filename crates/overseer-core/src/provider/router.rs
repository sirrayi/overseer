//! Deployment router (litellm router strategies, arsenal B2).
//!
//! When the same model is served by several deployments (a local server, a
//! gateway, a fallback region), *which* one to call is a policy. LiteLLM
//! ships named strategies for it; this port lands the five that are pure
//! functions over a deployment's observable state — no health checker, no
//! background poller, no HTTP:
//!
//! | strategy | picks |
//! |---|---|
//! | `SimpleShuffle` | next in rotation (round-robin stand-in) |
//! | `LeastBusy` | fewest in-flight requests |
//! | `LatencyBased` | lowest observed latency |
//! | `UsageBased` | lowest share of its rate limit in use |
//! | `CostBased` | cheapest per-Mtok |
//!
//! Ties always break deterministically (latency → name, then name), because
//! a router whose choice depends on iteration order makes an eval arm
//! irreproducible. `SimpleShuffle` is round-robin over a caller-supplied
//! rotation counter: litellm shuffles randomly, which is fine for load
//! spreading and useless for testing, so the caller owns the randomness and
//! the engine stays deterministic.
//! `// DEFERRED(owner): cooldown/failure tracking, weighted routing, and
//! per-deployment health polling (litellm's `allowed_fails`/`cooldown_time`)
//! — the strategy math lands here; the state machine needs the ops surface.`

/// One deployment of a model, with the stats a strategy may consider.
#[derive(Debug, Clone, PartialEq)]
pub struct Deployment {
    pub name: String,
    /// Observed latency in ms (0 = unmeasured).
    pub latency_ms: f64,
    /// Requests currently in flight.
    pub in_flight: u32,
    /// Price per million tokens, whichever direction the caller cares about.
    pub cost_per_mtok: f64,
    /// Requests consumed in the current rate-limit window.
    pub rpm_used: u32,
    /// Requests allowed per window (0 = unlimited).
    pub rpm_limit: u32,
}

impl Deployment {
    /// Share of the rate limit in use, 0.0 when the deployment is unlimited.
    pub fn usage_ratio(&self) -> f64 {
        if self.rpm_limit == 0 {
            return 0.0;
        }
        f64::from(self.rpm_used) / f64::from(self.rpm_limit)
    }
}

/// The strategies this port implements.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RouterStrategy {
    /// Round-robin: the caller supplies the rotation counter.
    SimpleShuffle,
    /// Fewest requests in flight.
    LeastBusy,
    /// Lowest observed latency.
    LatencyBased,
    /// Lowest share of its rate limit consumed.
    UsageBased,
    /// Cheapest per million tokens.
    CostBased,
}

impl RouterStrategy {
    pub const ALL: [RouterStrategy; 5] = [
        RouterStrategy::SimpleShuffle,
        RouterStrategy::LeastBusy,
        RouterStrategy::LatencyBased,
        RouterStrategy::UsageBased,
        RouterStrategy::CostBased,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            RouterStrategy::SimpleShuffle => "simple_shuffle",
            RouterStrategy::LeastBusy => "least_busy",
            RouterStrategy::LatencyBased => "latency_based",
            RouterStrategy::UsageBased => "usage_based",
            RouterStrategy::CostBased => "cost_based",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().to_ascii_lowercase();
        RouterStrategy::ALL
            .into_iter()
            .find(|c| c.as_str() == s || c.as_str().replace('_', "-") == s)
            .ok_or_else(|| {
                let names: Vec<&str> = RouterStrategy::ALL.iter().map(|c| c.as_str()).collect();
                format!("router: unknown strategy `{s}` — want {}", names.join("|"))
            })
    }
}

/// Pick a deployment index. `rotation` is only read by `SimpleShuffle`
/// (`usize::MAX`/monotonic counters are fine — it is taken modulo the
/// candidate count). `None` when there are no deployments.
pub fn route(
    strategy: RouterStrategy,
    deployments: &[Deployment],
    rotation: usize,
) -> Option<usize> {
    if deployments.is_empty() {
        return None;
    }
    match strategy {
        RouterStrategy::SimpleShuffle => Some(rotation % deployments.len()),
        RouterStrategy::LeastBusy => best(deployments, |d| f64::from(d.in_flight)),
        RouterStrategy::LatencyBased => best(deployments, |d| d.latency_ms),
        RouterStrategy::UsageBased => best(deployments, |d| d.usage_ratio()),
        RouterStrategy::CostBased => best(deployments, |d| d.cost_per_mtok),
    }
}

/// Index of the minimum, with deterministic tie-breaks: lowest latency, then
/// name order. `metric` must be finite (NaN would poison the comparison), so
/// non-finite values are treated as worst.
fn best(deployments: &[Deployment], metric: impl Fn(&Deployment) -> f64) -> Option<usize> {
    let mut chosen: Option<(usize, f64)> = None;
    for (i, d) in deployments.iter().enumerate() {
        let m = metric(d);
        let m = if m.is_finite() { m } else { f64::INFINITY };
        let take = match chosen {
            None => true,
            Some((ci, cm)) => {
                if m < cm {
                    true
                } else if m > cm {
                    false
                } else {
                    // Tie → lowest latency, then name.
                    let (a, b) = (&deployments[i], &deployments[ci]);
                    (a.latency_ms, a.name.as_str()) < (b.latency_ms, b.name.as_str())
                }
            }
        };
        if take {
            chosen = Some((i, m));
        }
    }
    chosen.map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dep(
        name: &str,
        latency: f64,
        in_flight: u32,
        cost: f64,
        used: u32,
        limit: u32,
    ) -> Deployment {
        Deployment {
            name: name.into(),
            latency_ms: latency,
            in_flight,
            cost_per_mtok: cost,
            rpm_used: used,
            rpm_limit: limit,
        }
    }

    fn fleet() -> Vec<Deployment> {
        vec![
            dep("gateway", 900.0, 1, 0.6, 90, 100),
            dep("local", 120.0, 4, 0.0, 10, 100),
            dep("fallback", 300.0, 1, 0.2, 50, 100),
        ]
    }

    #[test]
    fn each_strategy_picks_its_own_criterion() {
        let d = fleet();
        // least_busy: gateway and fallback tie at 1 in flight → latency
        // breaks it (gateway 900 vs fallback 300).
        assert_eq!(route(RouterStrategy::LeastBusy, &d, 0), Some(2));
        // latency_based: the local server.
        assert_eq!(route(RouterStrategy::LatencyBased, &d, 0), Some(1));
        // usage_based: 10% < 50% < 90%.
        assert_eq!(route(RouterStrategy::UsageBased, &d, 0), Some(1));
        // cost_based: free local server.
        assert_eq!(route(RouterStrategy::CostBased, &d, 0), Some(1));
        // simple_shuffle: round-robin over the rotation counter.
        assert_eq!(route(RouterStrategy::SimpleShuffle, &d, 0), Some(0));
        assert_eq!(route(RouterStrategy::SimpleShuffle, &d, 1), Some(1));
        assert_eq!(route(RouterStrategy::SimpleShuffle, &d, 7), Some(1));
        assert_eq!(route(RouterStrategy::CostBased, &[], 0), None);
    }

    #[test]
    fn ties_break_deterministically_and_unlimited_is_free() {
        // Identical twins: the alphabetically first name wins, whichever
        // order the vector is in.
        let a = dep("beta", 100.0, 0, 1.0, 0, 0);
        let b = dep("alpha", 100.0, 0, 1.0, 0, 0);
        assert_eq!(
            route(RouterStrategy::LeastBusy, &[a.clone(), b.clone()], 0),
            Some(1)
        );
        assert_eq!(
            route(RouterStrategy::LeastBusy, &[b.clone(), a.clone()], 0),
            Some(0)
        );
        // rpm_limit 0 = unlimited → usage 0.0, not a division by zero.
        assert_eq!(a.usage_ratio(), 0.0);
        let unlimited = dep("unlimited", 10.0, 9, 5.0, 999, 0);
        let limited = dep("limited", 10.0, 0, 5.0, 1, 10);
        assert_eq!(
            route(RouterStrategy::UsageBased, &[unlimited, limited], 0),
            Some(0)
        );
        // A non-finite metric is treated as worst, never as best.
        let nan = dep("nan", f64::NAN, 0, f64::NAN, 0, 10);
        let ok = dep("ok", 100.0, 0, 1.0, 0, 10);
        assert_eq!(
            route(RouterStrategy::LatencyBased, &[nan.clone(), ok.clone()], 0),
            Some(1)
        );
        assert_eq!(route(RouterStrategy::CostBased, &[nan, ok], 0), Some(1));
    }

    #[test]
    fn strategy_names_parse_and_round_trip() {
        for s in RouterStrategy::ALL {
            assert_eq!(RouterStrategy::parse(s.as_str()).unwrap(), s);
            assert_eq!(
                RouterStrategy::parse(&s.as_str().replace('_', "-")).unwrap(),
                s
            );
        }
        assert_eq!(
            RouterStrategy::parse(" Least-Busy ").unwrap(),
            RouterStrategy::LeastBusy
        );
        let err = RouterStrategy::parse("magic").unwrap_err();
        assert!(err.contains("magic"), "{err}");
        assert!(
            err.contains("least_busy"),
            "the error lists the choices: {err}"
        );
    }
}
