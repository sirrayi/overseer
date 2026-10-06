//! Stuck detector (playbook Ch.4 §3.4 — OpenHands' five patterns).
//! Runs inside the engine, not the prompt: it observes the action/observation
//! stream and trips on repetition the model can't see itself making.
//!
//! Patterns (OpenHands StuckDetector, adapted to our event log):
//!   1. Same (action, observation) pair 4×        — futile loop
//!   2. Same action erroring 3×                   — failing loop
//!   3. 3+ consecutive no-progress monologue turns — thinking without acting
//!      (can't fire in the default loop: a no-tool response ends the run;
//!      kept for future loops where steering can continue a turn)
//!   4. Alternating A-B-A-B over 6+ steps         — ping-pong
//!   5. Repeated context-window errors            — compaction failing

use std::collections::VecDeque;

/// What tripped. Reported verbatim into the event log + the nudge prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StuckPattern {
    ActionObservationLoop,
    ActionErrorLoop,
    Monologue,
    PingPong,
    ContextWindowLoop,
}

impl StuckPattern {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::ActionObservationLoop => "the same action produced the same observation 4 times",
            Self::ActionErrorLoop => "the same action has errored 3 times",
            Self::Monologue => "3+ turns of reasoning with no tool calls or progress",
            Self::PingPong => "alternating between two actions for 6+ steps",
            Self::ContextWindowLoop => "repeated context-window errors",
        }
    }
}

const HISTORY_CAP: usize = 32;
const SAME_PAIR_TRIP: usize = 4;
const SAME_ERROR_TRIP: usize = 3;
const MONOLOGUE_TRIP: usize = 3;
const PINGPONG_WINDOW: usize = 6;
const CONTEXT_ERR_TRIP: usize = 3;

/// Signature of one tool call: name + normalized input JSON.
fn action_sig(name: &str, input: &serde_json::Value) -> String {
    format!(
        "{name}:{}",
        serde_json::to_string(input).unwrap_or_default()
    )
}

/// Signature of an observation: error flag + a cheap content fingerprint
/// (len + head/tail — full hashing pays for size we never need).
fn obs_sig(is_error: bool, content: &str) -> String {
    let head: String = content.chars().take(200).collect();
    let tail: String = content
        .chars()
        .rev()
        .take(200)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("err={is_error};len={};{head}…{tail}", content.len())
}

#[derive(Default)]
pub struct StuckDetector {
    /// (action_sig, obs_sig) per tool call, in order.
    steps: VecDeque<(String, String)>,
    /// Consecutive model responses with no tool calls.
    monologue: usize,
    /// Consecutive context-window stop reasons.
    context_errors: usize,
}

impl StuckDetector {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget all history — called at the start of every user turn so one
    /// turn's repetition never trips the next.
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Record a completed tool call + its observation. Call once per pair.
    pub fn observe_step(
        &mut self,
        name: &str,
        input: &serde_json::Value,
        is_error: bool,
        content: &str,
    ) -> Option<StuckPattern> {
        self.steps
            .push_back((action_sig(name, input), obs_sig(is_error, content)));
        if self.steps.len() > HISTORY_CAP {
            self.steps.pop_front();
        }
        self.monologue = 0; // a tool call is progress by definition
        self.check()
    }

    /// Record a model response. `had_tool_calls` distinguishes text-only turns.
    pub fn observe_response(
        &mut self,
        had_tool_calls: bool,
        context_window_error: bool,
    ) -> Option<StuckPattern> {
        if had_tool_calls {
            self.monologue = 0;
        } else {
            self.monologue += 1;
        }
        if context_window_error {
            self.context_errors += 1;
        } else {
            self.context_errors = 0;
        }
        self.check()
    }

    fn check(&self) -> Option<StuckPattern> {
        if self.context_errors >= CONTEXT_ERR_TRIP {
            return Some(StuckPattern::ContextWindowLoop);
        }
        if self.monologue >= MONOLOGUE_TRIP {
            return Some(StuckPattern::Monologue);
        }
        if self.same_pair_loop() {
            return Some(StuckPattern::ActionObservationLoop);
        }
        if self.same_error_loop() {
            return Some(StuckPattern::ActionErrorLoop);
        }
        if self.ping_pong() {
            return Some(StuckPattern::PingPong);
        }
        None
    }

    /// P1: last N (action,obs) pairs identical.
    fn same_pair_loop(&self) -> bool {
        if self.steps.len() < SAME_PAIR_TRIP {
            return false;
        }
        let tail: Vec<_> = self.steps.iter().rev().take(SAME_PAIR_TRIP).collect();
        tail.iter().all(|s| *s == tail[0])
    }

    /// P2: last N steps share an action AND all errored.
    fn same_error_loop(&self) -> bool {
        if self.steps.len() < SAME_ERROR_TRIP {
            return false;
        }
        let tail: Vec<_> = self.steps.iter().rev().take(SAME_ERROR_TRIP).collect();
        let first_action = &tail[0].0;
        tail.iter()
            .all(|(a, o)| a == first_action && o.starts_with("err=true"))
    }

    /// P4: last 6+ steps alternate between exactly two (action,
    /// observation) pairs (A,B,A,B,A,B) — changing observations are
    /// progress, not a loop.
    fn ping_pong(&self) -> bool {
        if self.steps.len() < PINGPONG_WINDOW {
            return false;
        }
        let tail: Vec<_> = self.steps.iter().rev().take(PINGPONG_WINDOW).collect();
        let a = tail[0];
        let b = tail[1];
        a.0 != b.0
            && tail
                .iter()
                .enumerate()
                .all(|(i, s)| *s == if i % 2 == 0 { a } else { b })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn step(
        d: &mut StuckDetector,
        name: &str,
        input: serde_json::Value,
        err: bool,
        out: &str,
    ) -> Option<StuckPattern> {
        d.observe_step(name, &input, err, out)
    }

    #[test]
    fn fires_on_identical_loop() {
        let mut d = StuckDetector::new();
        let inp = json!({"command": "cargo test"});
        assert_eq!(step(&mut d, "bash", inp.clone(), false, "all ok"), None);
        assert_eq!(step(&mut d, "bash", inp.clone(), false, "all ok"), None);
        assert_eq!(step(&mut d, "bash", inp.clone(), false, "all ok"), None);
        assert_eq!(
            step(&mut d, "bash", inp, false, "all ok"),
            Some(StuckPattern::ActionObservationLoop)
        );
    }

    #[test]
    fn different_observations_do_not_trip() {
        let mut d = StuckDetector::new();
        let inp = json!({"command": "ls"});
        for i in 0..6 {
            let out = format!("file{i}");
            assert_eq!(step(&mut d, "bash", inp.clone(), false, &out), None);
        }
    }

    #[test]
    fn fires_on_error_loop() {
        let mut d = StuckDetector::new();
        let inp = json!({"path": "missing.rs"});
        step(&mut d, "read", inp.clone(), true, "no such file");
        step(&mut d, "read", inp.clone(), true, "no such file v2");
        assert_eq!(
            step(&mut d, "read", inp, true, "no such file v3"),
            Some(StuckPattern::ActionErrorLoop)
        );
    }

    #[test]
    fn fires_on_ping_pong() {
        let mut d = StuckDetector::new();
        let a = json!({"command": "make fix"});
        let b = json!({"command": "make revert"});
        let mut last = None;
        for i in 0..6 {
            last = if i % 2 == 0 {
                step(&mut d, "bash", a.clone(), false, "done a")
            } else {
                step(&mut d, "bash", b.clone(), false, "done b")
            };
        }
        assert_eq!(last, Some(StuckPattern::PingPong));
    }

    #[test]
    fn fires_on_context_errors() {
        let mut d = StuckDetector::new();
        assert_eq!(d.observe_response(false, true), None);
        assert_eq!(d.observe_response(false, true), None);
        assert_eq!(
            d.observe_response(false, true),
            Some(StuckPattern::ContextWindowLoop)
        );
    }

    #[test]
    fn normal_work_does_not_trip() {
        let mut d = StuckDetector::new();
        let calls = [
            ("glob", json!({"pattern": "**/*.rs"})),
            ("read", json!({"path": "a.rs"})),
            ("read", json!({"path": "b.rs"})),
            (
                "edit",
                json!({"path": "a.rs", "old_string": "x", "new_string": "y"}),
            ),
            ("bash", json!({"command": "cargo test"})),
            (
                "edit",
                json!({"path": "b.rs", "old_string": "p", "new_string": "q"}),
            ),
            ("bash", json!({"command": "cargo build"})),
        ];
        for (n, i) in calls {
            assert_eq!(step(&mut d, n, i, false, "ok"), None);
        }
    }
}
