//! Orchestration patterns ported from the workflow-engine batch (arsenal B2).
//!
//! Seven ports, all pure data-plus-decision logic — no runtime, no server, no
//! dependency:
//!
//! - **langgraph reducers** — how a state channel combines an update with its
//!   current value (`operator.add` and the last-value default, as a table).
//! - **crewai sub-roles** — hierarchical delegation with a cycle guard.
//! - **oai-handoff filter** — which handoff tools an agent may actually see.
//! - **smolagents `{{prev.N}}` refs** — step-output interpolation for a
//!   batched step list.
//! - **temporal activity** — the retry/timeout decision an activity wrapper
//!   makes before every attempt.
//! - **restate `durable_run`** — journaled step replay (pattern only: the
//!   runtime is BSL-1.1, so nothing is vendored or linked).
//! - **inngest `step_run`** — a durable memo keyed by run + step (pattern
//!   only: the server is SSPL/DOSP; the SDK shape is what is ported).
//!
//! `// DEFERRED(owner): actually *running* a durable workflow (journal
//! persistence, cron/event triggers, a worker loop) — these ports are the
//! decision layer the engine would drive; the runtimes stay out by license
//! policy and by scope.`

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;

/// How a state channel merges an update into its current value
/// (langgraph's reducer table).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reducer {
    /// Last write wins (langgraph's default channel).
    Last,
    /// Arrays concatenate, strings concatenate; anything else → update.
    Concat,
    /// Numbers add.
    Sum,
    /// Numbers take the larger value.
    Max,
    /// Arrays merge, deduplicated, first-seen order preserved.
    Union,
}

impl Reducer {
    pub const ALL: [Reducer; 5] = [
        Reducer::Last,
        Reducer::Concat,
        Reducer::Sum,
        Reducer::Max,
        Reducer::Union,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Reducer::Last => "last",
            Reducer::Concat => "concat",
            Reducer::Sum => "sum",
            Reducer::Max => "max",
            Reducer::Union => "union",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim().to_ascii_lowercase();
        Reducer::ALL
            .into_iter()
            .find(|r| r.as_str() == s)
            .ok_or_else(|| {
                let names: Vec<&str> = Reducer::ALL.iter().map(|r| r.as_str()).collect();
                format!("reducer: unknown `{s}` — want {}", names.join("|"))
            })
    }
}

/// Fold `update` into `current` under `reducer`.
///
/// A shape mismatch (adding a string to a number, concatenating an object)
/// is not an error: the update wins, which is langgraph's own behavior for a
/// channel whose value type changed. Silence here is deliberate — the
/// alternative is a workflow that dies on a schema drift it can absorb.
pub fn reduce(reducer: Reducer, current: &Value, update: &Value) -> Value {
    match reducer {
        Reducer::Last => update.clone(),
        Reducer::Concat => match (current, update) {
            (Value::Array(a), Value::Array(b)) => {
                let mut out = a.clone();
                out.extend(b.iter().cloned());
                Value::Array(out)
            }
            (Value::String(a), Value::String(b)) => Value::String(format!("{a}{b}")),
            _ => update.clone(),
        },
        Reducer::Sum => match (as_f64(current), as_f64(update)) {
            (Some(a), Some(b)) => number(a + b),
            _ => update.clone(),
        },
        Reducer::Max => match (as_f64(current), as_f64(update)) {
            (Some(a), Some(b)) => number(a.max(b)),
            _ => update.clone(),
        },
        Reducer::Union => match (current, update) {
            (Value::Array(a), Value::Array(b)) => {
                let mut out: Vec<Value> = Vec::with_capacity(a.len() + b.len());
                for v in a.iter().chain(b.iter()) {
                    if !out.contains(v) {
                        out.push(v.clone());
                    }
                }
                Value::Array(out)
            }
            _ => update.clone(),
        },
    }
}

fn as_f64(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// Integral results stay integral (a step counter should read `3`, not `3.0`).
fn number(x: f64) -> Value {
    if x.fract() == 0.0 && x.abs() < 9_007_199_254_740_992.0 {
        Value::from(x as i64)
    } else {
        Value::from(x)
    }
}

/// One role in a hierarchical crew (crewai's manager → coworker shape).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubRole {
    pub name: String,
    pub goal: String,
    /// Role names this role may hand work to.
    pub delegates_to: Vec<String>,
}

impl SubRole {
    pub fn new(name: &str, goal: &str, delegates_to: &[&str]) -> Self {
        SubRole {
            name: name.to_string(),
            goal: goal.to_string(),
            delegates_to: delegates_to.iter().map(|s| s.to_string()).collect(),
        }
    }
}

/// Whether `from` may delegate to `to`, with the two guards a delegation
/// graph needs: no self-delegation, and no cycle back through an existing
/// edge (a manager that can hand work back up its own chain is an infinite
/// review loop). Errors name the offending edge so the crew definition is
/// fixable from the message alone.
pub fn delegation_ok(roles: &[SubRole], from: &str, to: &str) -> Result<(), String> {
    if from == to {
        return Err(format!("crew: '{from}' cannot delegate to itself"));
    }
    let Some(src) = roles.iter().find(|r| r.name == from) else {
        return Err(format!("crew: unknown role '{from}'"));
    };
    if !src.delegates_to.iter().any(|d| d == to) {
        return Err(format!(
            "crew: '{from}' does not delegate to '{to}' (allowed: {})",
            if src.delegates_to.is_empty() {
                "(none)".to_string()
            } else {
                src.delegates_to.join(", ")
            }
        ));
    }
    if !roles.iter().any(|r| r.name == to) {
        return Err(format!("crew: unknown role '{to}'"));
    }
    // Cycle guard: can `to` already reach `from`? Then this edge closes a
    // loop and work handed down could be handed straight back up.
    if let Some(path) = path_to(roles, to, from) {
        return Err(format!(
            "crew: delegation cycle — {from} → {to} → … ({})",
            path.join(" → ")
        ));
    }
    Ok(())
}

/// BFS for a delegation path from `start` to `target` (the cycle witness).
fn path_to(roles: &[SubRole], start: &str, target: &str) -> Option<Vec<String>> {
    let mut queue: Vec<Vec<String>> = vec![vec![start.to_string()]];
    let mut seen: Vec<String> = vec![start.to_string()];
    while let Some(path) = queue.pop() {
        let last = path.last()?.clone();
        let Some(role) = roles.iter().find(|r| r.name == last) else {
            continue;
        };
        for next in &role.delegates_to {
            let mut p = path.clone();
            p.push(next.clone());
            if next == target {
                return Some(p);
            }
            if !seen.contains(next) {
                seen.push(next.clone());
                queue.push(p);
            }
        }
    }
    None
}

/// One declared handoff edge (OpenAI Agents SDK `handoff()` declarations).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Handoff {
    /// Agent that owns the tool.
    pub from: String,
    /// Agent the tool hands off to.
    pub to: String,
    /// Whether the edge is enabled (`handoff(..., enabled=False)`).
    pub enabled: bool,
}

/// The handoff tools an agent may actually see: its own edges, enabled, never
/// to itself, deduplicated by target in declaration order. The self-edge
/// filter matters because a self-handoff is a no-op tool that wastes a turn
/// and confuses the model about which agent it is.
pub fn handoff_tools<'a>(current: &str, all: &'a [Handoff]) -> Vec<&'a Handoff> {
    let mut out: Vec<&Handoff> = Vec::new();
    for h in all {
        if h.from != current || !h.enabled || h.to == current {
            continue;
        }
        if out.iter().any(|e| e.to == h.to) {
            continue;
        }
        out.push(h);
    }
    out
}

/// Interpolate `{{prev.N}}` references in a step's input template
/// (smolagents' batched-step shape). `N` is 1-based over the completed step
/// outputs; an out-of-range or malformed reference is an error naming the
/// valid range, so a mis-numbered batch fails loudly instead of sending the
/// literal `{{prev.7}}` to a tool.
pub fn resolve_prev_refs(template: &str, prev: &[String]) -> Result<String, String> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{prev.") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{{prev.".len()..];
        let Some(end) = after.find("}}") else {
            return Err(format!(
                "refs: unterminated `{{{{prev.…}}}}` reference in `{template}`"
            ));
        };
        let idx_raw = &after[..end];
        let idx: usize = idx_raw.trim().parse().map_err(|_| {
            format!("refs: bad reference `{{{{prev.{idx_raw}}}}}` — want a 1-based step number")
        })?;
        if idx == 0 || idx > prev.len() {
            return Err(format!(
                "refs: `{{{{prev.{idx}}}}}` is out of range — {} step output(s) available ({{{{prev.1}}}}..{{{{prev.{}}}}})",
                prev.len(),
                prev.len()
            ));
        }
        out.push_str(&prev[idx - 1]);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}

/// One retryable activity (temporal's `ActivityOptions`).
#[derive(Debug, Clone, PartialEq)]
pub struct Activity {
    pub name: String,
    /// Total attempts allowed, including the first (1 = no retries).
    pub max_attempts: u32,
    /// Per-attempt timeout in ms.
    pub timeout_ms: u64,
    /// First retry delay in ms.
    pub backoff_ms: u64,
    /// Multiplier applied per further attempt (>= 1.0).
    pub backoff_factor: f64,
}

/// What the wrapper does next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivityDecision {
    /// Run attempt `n` (0-based).
    Run,
    /// Sleep this long, then run again.
    RetryAfter(u64),
    /// Stop; the string is the reason (timeout / attempts exhausted).
    GiveUp(String),
}

/// Decide what to do before attempt `attempt` (0-based) given how long the
/// current attempt has been running. Order is deliberate: a timeout is
/// terminal even when attempts remain (retrying a hung activity just burns
/// the budget), and the attempt ceiling is checked last so an activity with
/// `max_attempts = 1` runs exactly once.
pub fn activity_decision(a: &Activity, attempt: u32, elapsed_ms: u64) -> ActivityDecision {
    if elapsed_ms > a.timeout_ms {
        return ActivityDecision::GiveUp(format!(
            "{}: attempt {attempt} timed out after {elapsed_ms}ms (limit {}ms)",
            a.name, a.timeout_ms
        ));
    }
    if attempt == 0 {
        return ActivityDecision::Run;
    }
    if attempt >= a.max_attempts {
        return ActivityDecision::GiveUp(format!(
            "{}: {attempt} attempt(s) exhausted (max_attempts {})",
            a.name, a.max_attempts
        ));
    }
    let factor = if a.backoff_factor.is_finite() && a.backoff_factor >= 1.0 {
        a.backoff_factor
    } else {
        1.0
    };
    let delay = a.backoff_ms as f64 * factor.powi(attempt as i32 - 1);
    // Saturation, not wraparound: a large factor must not produce a bogus
    // small delay (or a panic in the float→int cast).
    let delay = if delay.is_finite() && delay >= 0.0 {
        delay.min(u64::MAX as f64) as u64
    } else {
        a.backoff_ms
    };
    ActivityDecision::RetryAfter(delay)
}

/// A journaled run (restate's durable-execution pattern, ported from the
/// published semantics only — no code taken; the runtime is BSL-1.1).
///
/// A step whose name is already in the journal returns the recorded output
/// **without running the closure**, which is what makes a replay after a
/// crash produce the same result instead of re-performing side effects.
#[derive(Debug, Default, Clone)]
pub struct DurableRun {
    journal: BTreeMap<String, String>,
}

impl DurableRun {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rehydrate from a persisted journal (name → output), in order.
    pub fn from_journal(entries: &[(String, String)]) -> Self {
        DurableRun {
            journal: entries.iter().cloned().collect(),
        }
    }

    /// Run `step` unless it is already journaled. Returns the output either
    /// way.
    pub fn run_step(&mut self, name: &str, step: impl FnOnce() -> String) -> String {
        if let Some(done) = self.journal.get(name) {
            return done.clone();
        }
        let out = step();
        self.journal.insert(name.to_string(), out.clone());
        out
    }

    /// True when the next step of this name would be replayed, not run.
    pub fn is_replayed(&self, name: &str) -> bool {
        self.journal.contains_key(name)
    }

    /// The journal so far, in name order (BTreeMap: deterministic).
    pub fn journal(&self) -> Vec<(String, String)> {
        self.journal
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }
}

/// A durable step memo keyed by (run, step) (inngest's `step.run` shape,
/// ported from the SDK's published semantics — the server is SSPL/DOSP).
///
/// Difference from `DurableRun`, and the reason both exist: `DurableRun` is a
/// single run's live journal, while the memo is a *store* shared across runs,
/// so a retried execution of an old run replays instead of re-firing its
/// side effects. `step_plan` exposes which steps of a run are already
/// memoized, which is how a resumed execution knows where it stopped.
#[derive(Debug, Default, Clone)]
pub struct StepMemo {
    entries: HashMap<(String, String), String>,
    order: HashMap<String, Vec<String>>,
}

impl StepMemo {
    pub fn new() -> Self {
        Self::default()
    }

    /// Run `step_id` of `run_id` unless memoized. Returns
    /// `(output, from_memo)` so a caller can report replays.
    pub fn step_run(
        &mut self,
        run_id: &str,
        step_id: &str,
        step: impl FnOnce() -> String,
    ) -> (String, bool) {
        let key = (run_id.to_string(), step_id.to_string());
        if let Some(done) = self.entries.get(&key) {
            return (done.clone(), true);
        }
        let out = step();
        self.entries.insert(key, out.clone());
        let order = self.order.entry(run_id.to_string()).or_default();
        if !order.iter().any(|s| s == step_id) {
            order.push(step_id.to_string());
        }
        (out, false)
    }

    /// The run's step ids in first-execution order, with memoized flags.
    pub fn step_plan(&self, run_id: &str) -> Vec<(String, bool)> {
        self.order
            .get(run_id)
            .map(|steps| {
                steps
                    .iter()
                    .map(|s| {
                        (
                            s.clone(),
                            self.entries.contains_key(&(run_id.to_string(), s.clone())),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Backoff delay before attempt `attempt` (1-based): `base_ms *
/// factor^(attempt-1)`. `attempt = 0` returns `base_ms`; a non-finite or
/// sub-1.0 factor is clamped to 1.0 (no shrink); overflow saturates at
/// `u64::MAX` instead of wrapping.
pub fn retry_delay(attempt: u32, base_ms: u64, factor: f32) -> u64 {
    if attempt <= 1 {
        return base_ms;
    }
    let factor = if factor.is_finite() && factor >= 1.0 {
        factor
    } else {
        1.0
    };
    let exp = attempt.saturating_sub(1).min(i32::MAX as u32) as i32;
    let delay = base_ms as f64 * f64::from(factor).powi(exp);
    if delay.is_finite() && delay >= 0.0 {
        delay.min(u64::MAX as f64) as u64
    } else {
        u64::MAX
    }
}

/// Retry scope for a coordinator: how many attempts a step may take and how
/// the delay grows between them.
#[derive(Debug, Clone, PartialEq)]
pub struct RetryScope {
    /// Total attempts allowed, including the first (1 = no retries).
    pub max_attempts: u32,
    /// First retry delay in ms.
    pub base_ms: u64,
    /// Multiplier applied per further attempt (>= 1.0).
    pub factor: f32,
}

/// Whether the coordinator retries after attempt `attempt` (count of attempts
/// already made). A timeout is terminal even with attempts left; otherwise
/// retry while `attempt < max_attempts`.
pub fn coordinator_should_retry(scope: &RetryScope, attempt: u32, timeout_hit: bool) -> bool {
    if timeout_hit {
        return false;
    }
    attempt < scope.max_attempts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reducers_fold_updates_the_way_the_channel_declares() {
        // last: the update wins outright.
        assert_eq!(reduce(Reducer::Last, &json!(1), &json!(2)), json!(2));
        // concat: arrays append, strings concatenate.
        assert_eq!(
            reduce(Reducer::Concat, &json!([1, 2]), &json!([3])),
            json!([1, 2, 3])
        );
        assert_eq!(
            reduce(Reducer::Concat, &json!("ab"), &json!("cd")),
            json!("abcd")
        );
        // sum / max stay integral when the result is integral.
        assert_eq!(reduce(Reducer::Sum, &json!(2), &json!(3)), json!(5));
        assert_eq!(reduce(Reducer::Sum, &json!(2.5), &json!(0.25)), json!(2.75));
        assert_eq!(reduce(Reducer::Max, &json!(2), &json!(7)), json!(7));
        assert_eq!(reduce(Reducer::Max, &json!(9), &json!(7)), json!(9));
        // union: dedup, first-seen order preserved across both sides.
        assert_eq!(
            reduce(Reducer::Union, &json!(["a", "b"]), &json!(["b", "c"])),
            json!(["a", "b", "c"])
        );
        // Shape mismatch → the update wins (schema drift is absorbed).
        assert_eq!(reduce(Reducer::Sum, &json!("x"), &json!(3)), json!(3));
        assert_eq!(
            reduce(Reducer::Concat, &json!({"a": 1}), &json!([1])),
            json!([1])
        );
        // Reducer names round-trip, and the error lists the choices.
        for r in Reducer::ALL {
            assert_eq!(Reducer::parse(r.as_str()).unwrap(), r);
        }
        assert!(Reducer::parse("nope").unwrap_err().contains("union"));
    }

    #[test]
    fn delegation_refuses_self_and_cycles() {
        // An acyclic crew: every declared edge downward, one leaf.
        let roles = vec![
            SubRole::new("manager", "coordinate", &["writer"]),
            SubRole::new("writer", "draft", &["reviewer"]),
            SubRole::new("reviewer", "review", &[]),
        ];
        assert!(delegation_ok(&roles, "manager", "writer").is_ok());
        assert!(delegation_ok(&roles, "writer", "reviewer").is_ok());
        // Self-delegation.
        let err = delegation_ok(&roles, "manager", "manager").unwrap_err();
        assert!(err.contains("itself"), "{err}");
        // Not a declared edge (upward delegation is not a cycle here — it is
        // simply not an edge).
        let err = delegation_ok(&roles, "writer", "manager").unwrap_err();
        assert!(err.contains("does not delegate"), "{err}");
        assert!(
            err.contains("allowed: reviewer"),
            "lists what is allowed: {err}"
        );
        // Unknown roles are named.
        assert!(delegation_ok(&roles, "ghost", "writer")
            .unwrap_err()
            .contains("ghost"));
        assert!(delegation_ok(&roles, "manager", "ghost")
            .unwrap_err()
            .contains("ghost"));
        // The cycle guard: with `b → a` declared, `a → b` closes the loop.
        let cyclic = vec![
            SubRole::new("a", "top", &["b"]),
            SubRole::new("b", "bottom", &["a"]),
        ];
        let err = delegation_ok(&cyclic, "a", "b").unwrap_err();
        assert!(err.contains("cycle"), "{err}");
        assert!(err.contains("a → b"), "names the closing edge: {err}");
        assert!(err.contains("→ a"), "shows the witness path: {err}");
    }

    #[test]
    fn handoff_filter_keeps_only_the_agents_own_enabled_edges() {
        let all = vec![
            Handoff {
                from: "triage".into(),
                to: "returns".into(),
                enabled: true,
            },
            Handoff {
                from: "triage".into(),
                to: "refunds".into(),
                enabled: false,
            },
            Handoff {
                from: "triage".into(),
                to: "returns".into(),
                enabled: true,
            },
            Handoff {
                from: "triage".into(),
                to: "triage".into(),
                enabled: true,
            },
            Handoff {
                from: "returns".into(),
                to: "triage".into(),
                enabled: true,
            },
        ];
        let tools = handoff_tools("triage", &all);
        assert_eq!(tools.len(), 1, "one usable edge: {tools:?}");
        assert_eq!(tools[0].to, "returns", "duplicates collapse by target");
        // The other agent's own tool is its own.
        assert_eq!(handoff_tools("returns", &all).len(), 1);
        assert!(handoff_tools("nobody", &all).is_empty());
    }

    #[test]
    fn prev_refs_interpolate_and_fail_loudly() {
        let prev = vec!["first".to_string(), "second".to_string()];
        assert_eq!(
            resolve_prev_refs("a={{prev.1}} b={{prev.2}}", &prev).unwrap(),
            "a=first b=second"
        );
        assert_eq!(resolve_prev_refs("plain", &prev).unwrap(), "plain");
        // The same reference twice is fine (it is a substitution, not a pop).
        assert_eq!(
            resolve_prev_refs("{{prev.2}}-{{prev.2}}", &prev).unwrap(),
            "second-second"
        );
        let err = resolve_prev_refs("{{prev.3}}", &prev).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
        assert!(
            err.contains("{{prev.1}}..{{prev.2}}"),
            "names the range: {err}"
        );
        assert!(resolve_prev_refs("{{prev.0}}", &prev)
            .unwrap_err()
            .contains("out of range"));
        assert!(resolve_prev_refs("{{prev.x}}", &prev)
            .unwrap_err()
            .contains("bad reference"));
        assert!(resolve_prev_refs("{{prev.1", &prev)
            .unwrap_err()
            .contains("unterminated"));
        assert!(resolve_prev_refs("{{prev.1}}", &[])
            .unwrap_err()
            .contains("0 step output"));
    }

    #[test]
    fn activity_decision_orders_timeout_before_retry() {
        let a = Activity {
            name: "fetch".into(),
            max_attempts: 3,
            timeout_ms: 1_000,
            backoff_ms: 100,
            backoff_factor: 2.0,
        };
        assert_eq!(activity_decision(&a, 0, 0), ActivityDecision::Run);
        assert_eq!(
            activity_decision(&a, 1, 10),
            ActivityDecision::RetryAfter(100)
        );
        assert_eq!(
            activity_decision(&a, 2, 10),
            ActivityDecision::RetryAfter(200),
            "backoff multiplies per attempt"
        );
        // Attempts exhausted (attempt == max_attempts).
        match activity_decision(&a, 3, 10) {
            ActivityDecision::GiveUp(why) => assert!(why.contains("exhausted"), "{why}"),
            other => panic!("expected GiveUp, got {other:?}"),
        }
        // A timeout is terminal even with attempts left.
        match activity_decision(&a, 1, 5_000) {
            ActivityDecision::GiveUp(why) => assert!(why.contains("timed out"), "{why}"),
            other => panic!("expected GiveUp, got {other:?}"),
        }
        // max_attempts = 1 runs exactly once.
        let once = Activity {
            max_attempts: 1,
            ..a.clone()
        };
        assert_eq!(activity_decision(&once, 0, 0), ActivityDecision::Run);
        assert!(matches!(
            activity_decision(&once, 1, 0),
            ActivityDecision::GiveUp(_)
        ));
        // A nonsense factor saturates rather than wrapping or panicking.
        let wild = Activity {
            backoff_factor: f64::INFINITY,
            backoff_ms: u64::MAX / 2,
            max_attempts: 10,
            ..a.clone()
        };
        match activity_decision(&wild, 5, 0) {
            ActivityDecision::RetryAfter(ms) => assert!(ms > 0, "saturated, not wrapped"),
            other => panic!("expected RetryAfter, got {other:?}"),
        }
        // A sub-1.0 factor is clamped to no-shrink.
        let flat = Activity {
            backoff_factor: 0.1,
            ..a.clone()
        };
        assert_eq!(
            activity_decision(&flat, 1, 0),
            ActivityDecision::RetryAfter(100)
        );
    }

    #[test]
    fn durable_run_replays_journaled_steps_without_running_them() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let mut run = DurableRun::new();
        let first = run.run_step("charge", || {
            calls.set(calls.get() + 1);
            "receipt-1".to_string()
        });
        assert_eq!(first, "receipt-1");
        assert_eq!(calls.get(), 1);
        assert!(run.is_replayed("charge"));
        // Replay: the closure must NOT run again (the side effect is done).
        let again = run.run_step("charge", || {
            calls.set(calls.get() + 1);
            "receipt-2".to_string()
        });
        assert_eq!(again, "receipt-1", "journal wins over a fresh call");
        assert_eq!(calls.get(), 1, "no second side effect");
        // A new step still runs.
        let next = run.run_step("notify", || "sent".to_string());
        assert_eq!(next, "sent");
        assert_eq!(calls.get(), 1);
        assert_eq!(
            run.journal(),
            vec![
                ("charge".to_string(), "receipt-1".to_string()),
                ("notify".to_string(), "sent".to_string()),
            ]
        );
        // Rehydrating from a journal reproduces the replay decision.
        let rehydrated = DurableRun::from_journal(&run.journal());
        assert!(rehydrated.is_replayed("charge"));
        assert!(!rehydrated.is_replayed("missing"));
    }

    #[test]
    fn step_memo_is_keyed_by_run_and_reports_the_plan() {
        use std::cell::Cell;
        let calls = Cell::new(0);
        let mut memo = StepMemo::new();
        let (out, reused) = memo.step_run("run-1", "load", || {
            calls.set(calls.get() + 1);
            "rows".to_string()
        });
        assert_eq!(out, "rows");
        assert!(!reused);
        let (out2, reused2) = memo.step_run("run-1", "load", || {
            calls.set(calls.get() + 1);
            "different".to_string()
        });
        assert_eq!(out2, "rows", "the memo wins on a retry");
        assert!(reused2);
        assert_eq!(calls.get(), 1);
        // The same step id under another run is a different memo.
        let (out3, reused3) = memo.step_run("run-2", "load", || "other".to_string());
        assert_eq!(out3, "other");
        assert!(!reused3);
        assert_eq!(calls.get(), 1);
        // The plan shows where a resumed execution stopped.
        memo.step_run("run-1", "index", || "done".to_string());
        assert_eq!(
            memo.step_plan("run-1"),
            vec![("load".to_string(), true), ("index".to_string(), true),]
        );
        assert!(memo.step_plan("unknown-run").is_empty());
    }

    #[test]
    fn retry_delay_grows_geometrically_and_saturates() {
        assert_eq!(retry_delay(0, 100, 2.0), 100, "attempt 0 → base");
        assert_eq!(retry_delay(1, 100, 2.0), 100);
        assert_eq!(retry_delay(2, 100, 2.0), 200);
        assert_eq!(retry_delay(3, 100, 2.0), 400);
        assert_eq!(
            retry_delay(2, 100, 0.5),
            100,
            "sub-1.0 factor clamps to 1.0"
        );
        assert_eq!(
            retry_delay(2, 100, f32::NAN),
            100,
            "non-finite factor clamps to 1.0"
        );
        assert_eq!(
            retry_delay(2, 100, f32::INFINITY),
            100,
            "infinite factor clamps to 1.0"
        );
        assert_eq!(retry_delay(200, u64::MAX, 2.0), u64::MAX, "saturates");
        assert_eq!(retry_delay(200, 1_000, 10.0), u64::MAX, "saturates");
    }

    #[test]
    fn coordinator_retry_stops_on_timeout_or_ceiling() {
        let scope = RetryScope {
            max_attempts: 3,
            base_ms: 100,
            factor: 2.0,
        };
        assert!(coordinator_should_retry(&scope, 0, false));
        assert!(coordinator_should_retry(&scope, 2, false));
        assert!(
            !coordinator_should_retry(&scope, 3, false),
            "ceiling reached"
        );
        assert!(!coordinator_should_retry(&scope, 4, false));
        assert!(
            !coordinator_should_retry(&scope, 0, true),
            "timeout terminal"
        );
        assert!(!coordinator_should_retry(&scope, 2, true));
        let once = RetryScope {
            max_attempts: 1,
            base_ms: 50,
            factor: 2.0,
        };
        assert!(coordinator_should_retry(&once, 0, false));
        assert!(!coordinator_should_retry(&once, 1, false));
    }
}
