//! Textual-gradient (textgrad) bounded refinement loop — the engine-side half.
//!
//! textgrad's idea is a *textual* gradient: measure a loss, ask an optimizer
//! for an improved candidate, measure again, repeat. Everything model-shaped
//! lives outside this module. What is ported here is the loop that owns the
//! bounds: `loss(text) → update(text)`, iterated under a hard round cap and a
//! strict-improvement rule, ending with an explicit stop reason.
//!
//! The scorer is injected. A [`Refiner`] supplies `loss` (an eval rig — this
//! repo's `evals/` is exactly such a caller) and `step` (the model/optimizer
//! turn). Injecting the loss is what makes the loop testable and replayable:
//! the same script produces the same outcome, with no model in the test.
//!
//! Why the bounds are not optional: every round is a model call, so a
//! refinement loop with no cap is a spend bug. [`validate_bounds`] refuses a
//! configuration that cannot terminate meaningfully, and the
//! strict-improvement rule stops a loop that has plateaued (or started
//! regressing) instead of burning rounds on it.
//!
//! `// DEFERRED(owner): the model-backed optimizer/backward pass (textgrad's
//! actual gradient) and wiring this loop into the turn loop's critique path
//! (B1-7 Reflexion owns that today) — the bounded text-level loop and its stop
//! conditions land now.`

/// The bounded round cap: a refinement loop must be able to terminate, and a
/// cap of zero would mean "run no refinement at all" — a configuration worth
/// refusing rather than silently honouring.
const MIN_ROUNDS: u32 = 1;

/// Upper bound on accepted rounds. Each round costs at least one model call
/// (the `step`), so the ceiling is deliberately small enough that a runaway
/// optimizer cannot silently drain a budget.
const MAX_ROUNDS: u32 = 20;

/// The two knobs a refinement loop is allowed to have.
///
/// Invariants (enforced by [`validate_bounds`]):
/// - `max_rounds` is in `1..=20` — bounded by construction;
/// - `min_improvement` is finite and `>= 0` — it is compared against a
///   difference of losses, so a NaN or negative threshold would make the
///   comparison meaningless.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bounds {
    /// Maximum number of *accepted* rounds. Rejected candidates do not consume
    /// a round: a rejected round stops the loop immediately.
    pub max_rounds: u32,
    /// Improvement that must be *strictly* exceeded for a candidate to be
    /// accepted. `0.0` means "any strict improvement; a tie is a plateau that
    /// stops the loop".
    pub min_improvement: f64,
}

/// Why the loop stopped. Every return from [`refine`] carries one of these, so
/// the caller never has to infer the outcome from the round count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// The best text measured `<= target`.
    TargetReached,
    /// The candidate did not strictly beat the current best by more than
    /// `min_improvement` (or its loss was non-finite). The better text is kept.
    NoImprovement,
    /// `max_rounds` accepted rounds elapsed without reaching the target.
    RoundsExhausted,
    /// The very first measurement was NaN or infinite; the loop never
    /// iterated. Carries the original text untouched.
    NonFiniteLoss,
}

/// What a run produced: the best text seen, how many rounds were accepted, the
/// accepted loss trajectory, and why it stopped.
///
/// Invariant (asserted by the tests): `losses.len() == rounds as usize + 1`.
/// `losses[0]` is the initial measurement; each accepted round appends exactly
/// one entry. Rejected candidates are deliberately *not* recorded, because
/// `losses` describes the trajectory of the returned (best) text, not every
/// measurement taken.
#[derive(Debug, Clone, PartialEq)]
pub struct RefineOutcome {
    /// The best text measured. For [`StopReason::NoImprovement`] this is the
    /// text *before* the rejected candidate — the loop never regresses to a
    /// worse text.
    pub text: String,
    /// Number of accepted rounds. `0` whenever the first measurement already
    /// met the target or was non-finite.
    pub rounds: u32,
    /// The accepted loss trajectory, oldest first; always `rounds + 1` long.
    pub losses: Vec<f64>,
    /// The stop condition, as documented on [`StopReason`].
    pub stop: StopReason,
}

/// The injected scorer/optimizer.
///
/// `loss` returns the value to minimize: lower is better, and it must be
/// finite for the loop to trust it. `step` produces a candidate text from the
/// current text and its loss — the caller owns whatever the optimizer turn
/// actually is; this module only bounds how often and on what it is called.
///
/// Both methods return `Result<_, String>`: an error propagates verbatim out
/// of [`refine`], so a model/rig failure is never mistaken for a bad candidate.
pub trait Refiner {
    /// Score `text`; lower is better. Non-finite values are handled by
    /// [`refine`] (a NaN on the first measurement stops the loop; a NaN from a
    /// candidate rejects that candidate).
    fn loss(&self, text: &str) -> Result<f64, String>;

    /// Produce a candidate from `text` and its `loss`. `text` must be
    /// non-empty in the result; an empty candidate is refused by [`refine`]
    /// with an error naming the invariant.
    fn step(&self, text: &str, loss: f64) -> Result<String, String>;
}

/// Refuse a `Bounds` that permits no useful refinement.
///
/// Errors name the offending field and its range so the configuration is
/// fixable from the message alone. Nothing is clamped or defaulted: silently
/// substituting a valid cap would hide a caller that meant something else.
pub fn validate_bounds(b: &Bounds) -> Result<(), String> {
    if b.max_rounds < MIN_ROUNDS || b.max_rounds > MAX_ROUNDS {
        return Err(format!(
            "refine: bounds.max_rounds = {} is out of range {}..={} \
             (a refinement loop is bounded by construction — every round is a \
             model call, so zero rounds is a no-op and an unbounded cap is a \
             spend bug)",
            b.max_rounds, MIN_ROUNDS, MAX_ROUNDS
        ));
    }
    if !b.min_improvement.is_finite() || b.min_improvement < 0.0 {
        return Err(format!(
            "refine: bounds.min_improvement = {} is not a finite value >= 0 \
             (it is compared against a difference of losses, so a NaN or \
             negative threshold cannot express an improvement)",
            b.min_improvement
        ));
    }
    Ok(())
}

/// Run the bounded refinement loop: measure, update, accept only real progress,
/// stop for a stated reason.
///
/// Order and rules, each deliberate:
/// - the initial measurement happens first, so a text that already meets the
///   target costs zero `step` calls;
/// - a non-finite initial loss stops immediately with
///   [`StopReason::NonFiniteLoss`] and the *original* text — iterating on a NaN
///   never converges (the classic infinite loop) and any candidate would be
///   judged against a meaningless baseline;
/// - a candidate is accepted only when its loss is finite **and**
///   `previous - next > min_improvement`, STRICTLY greater. An improvement
///   exactly equal to `min_improvement` means the loop has stopped making
///   progress — accepting it would spend rounds on a plateau — so it stops as
///   [`StopReason::NoImprovement`];
/// - a rejected candidate (worse, tied, or non-finite) ends the loop rather
///   than retrying, because `step` is a model call and a rejected round says
///   the optimizer has run out of ideas;
/// - on acceptance the text is replaced and, if the new loss meets the target,
///   the loop stops as [`StopReason::TargetReached`] *before* the cap is
///   consulted (a target hit on the final allowed round is still a success);
/// - otherwise the loop stops as [`StopReason::RoundsExhausted`] after
///   `max_rounds` accepted rounds.
///
/// The returned text is the best one seen, never the last candidate: a
/// regression can never be handed back. It is never empty — an empty `step`
/// output is an error naming the invariant, because the caller must never be
/// handed nothing to work with. A [`Refiner`] error propagates verbatim.
pub fn refine(
    initial: &str,
    target: f64,
    bounds: Bounds,
    r: &impl Refiner,
) -> Result<RefineOutcome, String> {
    validate_bounds(&bounds)?;

    let first = r.loss(initial)?;
    let mut losses = vec![first];

    if !first.is_finite() {
        return Ok(RefineOutcome {
            text: initial.to_string(),
            rounds: 0,
            losses,
            stop: StopReason::NonFiniteLoss,
        });
    }
    if first <= target {
        return Ok(RefineOutcome {
            text: initial.to_string(),
            rounds: 0,
            losses,
            stop: StopReason::TargetReached,
        });
    }

    let mut text = initial.to_string();
    let mut best = first;
    let mut rounds = 0u32;

    while rounds < bounds.max_rounds {
        let candidate = r.step(&text, best)?;
        if candidate.is_empty() {
            return Err(format!(
                "refine: the Refiner returned an empty text at round {} \
                 (the loop never returns an empty text — the caller must never \
                 be handed nothing)",
                rounds + 1
            ));
        }
        let next = r.loss(&candidate)?;

        // Strictly greater, and finite. A candidate whose loss is NaN or inf
        // is rejected exactly like a tie: the loop has no evidence of progress.
        if !(next.is_finite() && best - next > bounds.min_improvement) {
            return Ok(RefineOutcome {
                text,
                rounds,
                losses,
                stop: StopReason::NoImprovement,
            });
        }

        text = candidate;
        best = next;
        rounds += 1;
        losses.push(next);

        if next <= target {
            return Ok(RefineOutcome {
                text,
                rounds,
                losses,
                stop: StopReason::TargetReached,
            });
        }
    }

    Ok(RefineOutcome {
        text,
        rounds,
        losses,
        stop: StopReason::RoundsExhausted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A scripted refiner: `loss` looks the text up in a table (defaulting to
    /// "unknown" → the caller's choice), and `step` walks a scripted list.
    /// Counters make "never called" assertions exact.
    struct Script {
        /// text -> loss, consulted by `loss`.
        losses: Vec<(String, f64)>,
        /// Candidate returned by the nth `step` call.
        steps: Vec<String>,
        default_loss: f64,
        loss_calls: RefCell<u32>,
        step_calls: RefCell<u32>,
    }

    impl Script {
        fn new(losses: &[(&str, f64)], steps: &[&str], default_loss: f64) -> Self {
            Script {
                losses: losses.iter().map(|(t, l)| (t.to_string(), *l)).collect(),
                steps: steps.iter().map(|s| s.to_string()).collect(),
                default_loss,
                loss_calls: RefCell::new(0),
                step_calls: RefCell::new(0),
            }
        }

        fn loss_calls(&self) -> u32 {
            *self.loss_calls.borrow()
        }

        fn step_calls(&self) -> u32 {
            *self.step_calls.borrow()
        }
    }

    impl Refiner for Script {
        fn loss(&self, text: &str) -> Result<f64, String> {
            *self.loss_calls.borrow_mut() += 1;
            Ok(self
                .losses
                .iter()
                .find(|(t, _)| t == text)
                .map(|(_, l)| *l)
                .unwrap_or(self.default_loss))
        }

        fn step(&self, _text: &str, _loss: f64) -> Result<String, String> {
            let n = *self.step_calls.borrow_mut();
            *self.step_calls.borrow_mut() = n + 1;
            Ok(self
                .steps
                .get(n as usize)
                .cloned()
                .unwrap_or_else(|| format!("candidate-{}", n + 1)))
        }
    }

    /// A refiner that always errors, to prove propagation.
    struct Boom;

    impl Refiner for Boom {
        fn loss(&self, _text: &str) -> Result<f64, String> {
            Err("loss backend exploded".into())
        }
        fn step(&self, _text: &str, _loss: f64) -> Result<String, String> {
            Err("step backend exploded".into())
        }
    }

    fn bounds(max_rounds: u32, min_improvement: f64) -> Bounds {
        Bounds {
            max_rounds,
            min_improvement,
        }
    }

    /// The invariant every outcome must uphold, so no test has to restate it.
    fn assert_invariant(o: &RefineOutcome) {
        assert_eq!(
            o.losses.len(),
            o.rounds as usize + 1,
            "losses.len() == rounds + 1: {o:?}"
        );
        assert!(!o.text.is_empty(), "the returned text is never empty");
    }

    #[test]
    fn target_reached_on_first_measure_makes_zero_rounds_and_never_steps() {
        let s = Script::new(&[("draft", 0.2)], &[], 9.9);
        let o = refine("draft", 0.5, bounds(5, 0.0), &s).unwrap();
        assert_eq!(o.stop, StopReason::TargetReached);
        assert_eq!(o.rounds, 0);
        assert_eq!(o.losses, vec![0.2]);
        assert_eq!(o.text, "draft");
        assert_eq!(s.step_calls(), 0, "a met target costs no model call");
        assert_eq!(s.loss_calls(), 1, "exactly the initial measurement");
        assert_invariant(&o);
    }

    #[test]
    fn target_reached_after_two_accepted_rounds() {
        let s = Script::new(
            &[("draft", 1.0), ("v1", 0.6), ("v2", 0.1)],
            &["v1", "v2"],
            9.9,
        );
        let o = refine("draft", 0.5, bounds(5, 0.0), &s).unwrap();
        assert_eq!(o.stop, StopReason::TargetReached);
        assert_eq!(o.rounds, 2);
        assert_eq!(o.losses, vec![1.0, 0.6, 0.1]);
        assert_eq!(o.losses.len(), 3);
        assert_eq!(o.text, "v2");
        assert_eq!(s.step_calls(), 2);
        assert_invariant(&o);
    }

    #[test]
    fn improvement_exactly_at_the_threshold_is_a_plateau_and_stops() {
        // 1.0 → 0.9 is an improvement of exactly min_improvement (0.1).
        let s = Script::new(&[("draft", 1.0), ("v1", 0.9)], &["v1"], 9.9);
        let o = refine("draft", 0.0, bounds(5, 0.1), &s).unwrap();
        assert_eq!(o.stop, StopReason::NoImprovement, "{o:?}");
        assert_eq!(o.rounds, 0);
        assert_eq!(o.losses, vec![1.0], "a rejected candidate is not recorded");
        assert_eq!(o.text, "draft", "the better text is kept");
        assert_invariant(&o);
    }

    #[test]
    fn improvement_just_past_the_threshold_is_accepted() {
        // 1.0 → 0.899 beats the 0.1 threshold by a hair and is accepted.
        let s = Script::new(
            &[("draft", 1.0), ("v1", 0.899), ("v2", 9.0)],
            &["v1", "v2"],
            9.0,
        );
        let o = refine("draft", 0.0, bounds(5, 0.1), &s).unwrap();
        assert_eq!(o.stop, StopReason::NoImprovement);
        assert_eq!(o.rounds, 1);
        assert_eq!(o.losses, vec![1.0, 0.899]);
        assert_eq!(o.text, "v1");
        assert_invariant(&o);
    }

    #[test]
    fn a_regression_step_keeps_the_previous_text() {
        let s = Script::new(&[("draft", 1.0), ("worse", 2.0)], &["worse"], 9.9);
        let o = refine("draft", 0.0, bounds(5, 0.0), &s).unwrap();
        assert_eq!(o.stop, StopReason::NoImprovement);
        assert_eq!(o.rounds, 0);
        assert_eq!(o.text, "draft", "never regress to the worse candidate");
        assert_eq!(o.losses, vec![1.0]);
        assert_invariant(&o);
    }

    #[test]
    fn non_finite_initial_loss_stops_before_any_step() {
        let s = Script::new(&[("draft", f64::NAN)], &["v1"], 9.9);
        let o = refine("draft", 0.5, bounds(5, 0.0), &s).unwrap();
        assert_eq!(o.stop, StopReason::NonFiniteLoss);
        assert_eq!(o.rounds, 0);
        assert_eq!(o.text, "draft", "the original text, untouched");
        assert_eq!(o.losses.len(), 1);
        assert!(o.losses[0].is_nan());
        assert_eq!(s.step_calls(), 0, "never iterate on a NaN");
        assert_invariant(&o);
    }

    #[test]
    fn infinite_initial_loss_is_also_non_finite() {
        let s = Script::new(&[("draft", f64::INFINITY)], &["v1"], 9.9);
        let o = refine("draft", 0.5, bounds(5, 0.0), &s).unwrap();
        assert_eq!(o.stop, StopReason::NonFiniteLoss);
        assert_eq!(s.step_calls(), 0);
        assert_invariant(&o);
    }

    #[test]
    fn non_finite_step_loss_is_not_an_improvement() {
        let s = Script::new(&[("draft", 1.0), ("v1", f64::NAN)], &["v1"], 9.9);
        let o = refine("draft", 0.0, bounds(5, 0.0), &s).unwrap();
        assert_eq!(o.stop, StopReason::NoImprovement);
        assert_eq!(o.rounds, 0);
        assert_eq!(o.text, "draft");
        assert_eq!(o.losses, vec![1.0], "the NaN is not recorded");
        assert_invariant(&o);
    }

    #[test]
    fn rounds_exhausted_at_the_cap_with_correctly_sized_losses() {
        // Every step improves, none reaches the 0.0 target.
        let s = Script::new(
            &[("draft", 1.0), ("v1", 0.8), ("v2", 0.6), ("v3", 0.4)],
            &["v1", "v2", "v3"],
            9.9,
        );
        let o = refine("draft", 0.0, bounds(3, 0.1), &s).unwrap();
        assert_eq!(o.stop, StopReason::RoundsExhausted);
        assert_eq!(o.rounds, 3, "the cap counts accepted rounds");
        assert_eq!(o.losses, vec![1.0, 0.8, 0.6, 0.4]);
        assert_eq!(o.losses.len(), 4);
        assert_eq!(o.text, "v3");
        assert_eq!(s.step_calls(), 3, "exactly max_rounds model calls");
        assert_invariant(&o);
    }

    #[test]
    fn hitting_the_target_on_the_last_allowed_round_beats_the_cap() {
        let s = Script::new(
            &[("draft", 1.0), ("v1", 0.7), ("v2", 0.2)],
            &["v1", "v2"],
            9.9,
        );
        let o = refine("draft", 0.3, bounds(2, 0.1), &s).unwrap();
        assert_eq!(o.stop, StopReason::TargetReached);
        assert_eq!(o.rounds, 2);
        assert_eq!(o.losses, vec![1.0, 0.7, 0.2]);
        assert_invariant(&o);
    }

    #[test]
    fn bounds_reject_rounds_outside_range_and_name_the_field() {
        for bad in [0u32, 21, 100] {
            let err = validate_bounds(&bounds(bad, 0.0)).unwrap_err();
            assert!(err.contains("max_rounds"), "names the field: {err}");
            assert!(err.contains("1..=20"), "names the range: {err}");
            assert!(err.contains(&bad.to_string()), "names the value: {err}");
        }
        validate_bounds(&bounds(1, 0.0)).expect("1 is the minimum valid cap");
        validate_bounds(&bounds(20, 0.0)).expect("20 is the maximum valid cap");
    }

    #[test]
    fn bounds_reject_non_finite_or_negative_improvement_and_name_the_field() {
        for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, -0.1] {
            let err = validate_bounds(&bounds(3, bad)).unwrap_err();
            assert!(err.contains("min_improvement"), "names the field: {err}");
            assert!(err.contains("finite"), "names the requirement: {err}");
        }
        validate_bounds(&bounds(3, 0.0)).expect("0.0 means any strict improvement");
        validate_bounds(&bounds(3, 1e-9)).expect("a tiny positive threshold is valid");
    }

    #[test]
    fn an_empty_step_output_is_an_error_naming_the_invariant() {
        let s = Script::new(&[("draft", 1.0)], &[""], 9.9);
        let err = refine("draft", 0.0, bounds(3, 0.0), &s).unwrap_err();
        assert!(err.contains("empty"), "names the invariant: {err}");
        assert!(err.contains("round 1"), "names the round: {err}");
    }

    #[test]
    fn refiner_errors_propagate_verbatim() {
        let loss_err = refine("draft", 0.5, bounds(3, 0.0), &Boom).unwrap_err();
        assert_eq!(loss_err, "loss backend exploded");

        // A step failure, with a working initial loss, propagates too.
        struct StepBoom;
        impl Refiner for StepBoom {
            fn loss(&self, _text: &str) -> Result<f64, String> {
                Ok(1.0)
            }
            fn step(&self, _text: &str, _loss: f64) -> Result<String, String> {
                Err("optimizer unavailable".into())
            }
        }
        let step_err = refine("draft", 0.0, bounds(3, 0.0), &StepBoom).unwrap_err();
        assert_eq!(step_err, "optimizer unavailable");
    }

    #[test]
    fn bounds_are_validated_before_the_refiner_is_touched() {
        let s = Script::new(&[("draft", 1.0)], &["v1"], 9.9);
        let err = refine("draft", 0.0, bounds(0, 0.0), &s).unwrap_err();
        assert!(err.contains("max_rounds"));
        assert_eq!(s.loss_calls(), 0, "an invalid bounds never calls the rig");
        assert_eq!(s.step_calls(), 0);
    }

    #[test]
    fn losses_trace_the_best_text_and_never_include_a_rejected_candidate() {
        // Round 1 accepted, round 2 rejected: the outcome carries the round-1
        // text and a 2-long loss trajectory.
        let s = Script::new(
            &[("draft", 1.0), ("good", 0.5), ("worse", 0.9)],
            &["good", "worse"],
            9.9,
        );
        let o = refine("draft", 0.0, bounds(5, 0.05), &s).unwrap();
        assert_eq!(o.stop, StopReason::NoImprovement);
        assert_eq!(o.text, "good");
        assert_eq!(o.losses, vec![1.0, 0.5]);
        assert_eq!(o.losses.len(), o.rounds as usize + 1);
        assert_invariant(&o);
    }

    #[test]
    fn the_same_script_yields_the_same_outcome_twice() {
        let run = || {
            let s = Script::new(
                &[("draft", 1.0), ("v1", 0.7), ("v2", 0.4)],
                &["v1", "v2"],
                9.9,
            );
            let o = refine("draft", 0.0, bounds(2, 0.1), &s).unwrap();
            (o, s.step_calls(), s.loss_calls())
        };
        let (a, sa, la) = run();
        let (b, sb, lb) = run();
        assert_eq!(a, b, "same script → same outcome");
        assert_eq!((sa, la), (sb, lb), "same call counts");
        assert_eq!(a.stop, StopReason::RoundsExhausted);
        assert_invariant(&a);
    }
}
