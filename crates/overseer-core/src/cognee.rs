//! Pipeline-stage pattern ported from **cognee** (arsenal B2).
//!
//! cognee builds a *task graph*: named stages that consume keys produced by
//! earlier stages, run in declaration order, and carry a per-stage trace so
//! a partial run can be explained after the fact. This port keeps exactly
//! that contract and drops the runtime: a [`Stage`] is a pure
//! `Value -> Result<Value, String>` step, [`Pipeline`] runs stages
//! sequentially in declaration order, and the first failure halts the run —
//! later stages never execute, so a pipeline cannot half-apply a later step
//! on top of a failed earlier one.
//!
//! Two properties the rest of the crate relies on:
//!
//! - **Determinism.** Traces, trace `detail`, and [`Pipeline::describe`] are
//!   derived from declaration order and output *shape* only, never from map
//!   iteration order or timing, so a run is reproducible byte-for-byte.
//! - **Type agnosticism.** Stages receive and return `serde_json::Value`;
//!   a stage that returns a non-object passes through unmodified. The
//!   pipeline deliberately does not require object-in/object-out: only the
//!   stages that *need* an object (e.g. [`RequireKey`], [`SetKey`]) reject a
//!   non-object, and they say so in their own error.
//!
//! [`validate_pipeline`] checks the graph offline (unique names, every
//! consumed key produced by an earlier stage) so a broken graph is rejected
//! before any stage runs.
//! `// DEFERRED(owner): a real cognee task-graph executor (parallel
//! fan-out/fan-in, memoized re-runs, persistent graph state) — this batch
//! lands the sequential stage contract, the traces, and offline graph
//! validation only.`
//!
//! Landed since: [`status_report`] (pipeline_status observability) and
//! [`ForgetTombstone`] / [`tombstone`] / [`apply_forget`] (deterministic
//! forget key-stripper). Still deferred: parallel fan-out/fan-in,
//! memoized re-runs, persistent graph state.

use std::fmt;

use serde_json::Value;

/// One pipeline step. `name` is a `&'static str` because traces and errors
/// outlive the stage and must not borrow it: the name is stable for the
/// lifetime of the process, so a recorded trace stays readable after the
/// pipeline is dropped.
pub trait Stage {
    /// Stable identity of this stage as it appears in traces and errors.
    fn name(&self) -> &'static str;

    /// Transform `input`. `Err(reason)` aborts the whole pipeline; the
    /// reason is surfaced verbatim to the caller, so it should name the
    /// offending key/field and the repair, not just "invalid".
    fn run(&mut self, input: Value) -> Result<Value, String>;
}

/// Outcome of one stage. A failed stage's trace is still recorded (with
/// `ok: false` and the reason as `detail`) so the error can return the whole
/// partial history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageTrace {
    pub name: &'static str,
    pub ok: bool,
    pub detail: String,
}

/// A pipeline that stopped at `stage` (its declaration `index`, zero-based).
/// `traces` holds every stage up to and including the failure, in run order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PipelineError {
    pub stage: &'static str,
    pub index: usize,
    pub reason: String,
    pub traces: Vec<StageTrace>,
}

impl PipelineError {
    /// The stage names up to and including the failure, in order — the same
    /// list [`Pipeline::describe`] prints, truncated at the halt point.
    /// Lets a caller show "where did it stop" without re-walking the traces.
    pub fn halted_at(&self) -> Vec<&'static str> {
        self.traces.iter().map(|t| t.name).collect()
    }
}

impl fmt::Display for PipelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "cognee: stage `{}` (index {}) failed — {}",
            self.stage, self.index, self.reason
        )
    }
}

impl std::error::Error for PipelineError {}

/// A completed run: the final stage's output plus every stage's trace, in
/// declaration order. `traces.len()` equals the pipeline length on success.
#[derive(Debug, Clone, PartialEq)]
pub struct RunReport {
    pub output: Value,
    pub traces: Vec<StageTrace>,
}

/// Ordered list of stages. Order is the only scheduling input: stage `n`
/// sees exactly the output of stage `n - 1` (or the caller's input for the
/// first stage). No stage sees a sibling's output, and no stage runs twice.
#[derive(Default)]
pub struct Pipeline {
    stages: Vec<Box<dyn Stage>>,
}

impl Pipeline {
    /// Empty pipeline. Running it returns the input unchanged (the identity
    /// pipeline — a valid degenerate case, not an error).
    pub fn new() -> Self {
        Pipeline { stages: Vec::new() }
    }

    /// Append a stage to the end of the run order.
    pub fn push(&mut self, stage: Box<dyn Stage>) {
        self.stages.push(stage);
    }

    /// Number of stages, i.e. how many `run` calls a successful run makes.
    pub fn len(&self) -> usize {
        self.stages.len()
    }

    /// `true` for a pipeline with no stages. Present so the empty pipeline
    /// is a first-class case rather than a clippy-shaped afterthought.
    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    /// Declaration-order listing: `a → b → c`, or `(empty)` for no stages.
    /// Deterministic — it reads only the stage names, in order.
    pub fn describe(&self) -> String {
        if self.stages.is_empty() {
            return "(empty)".to_string();
        }
        self.stages
            .iter()
            .map(|s| s.name())
            .collect::<Vec<_>>()
            .join(" → ")
    }

    /// Run every stage in declaration order, feeding each stage the previous
    /// stage's output. The first `Err` halts the run: later stages do not
    /// execute, the error carries the failing stage's name, its index, the
    /// reason, and the traces recorded so far (the failing stage's own trace
    /// is the last one, `ok: false`).
    ///
    /// Errors are wrapped, never swallowed: a stage's `Err` text reaches the
    /// caller untouched apart from the stage/index prefix added by
    /// `PipelineError`'s `Display`.
    pub fn run(&mut self, input: Value) -> Result<RunReport, PipelineError> {
        let mut traces: Vec<StageTrace> = Vec::with_capacity(self.stages.len());
        let mut current = input;
        for (index, stage) in self.stages.iter_mut().enumerate() {
            match stage.run(current) {
                Ok(next) => {
                    traces.push(StageTrace {
                        name: stage.name(),
                        ok: true,
                        detail: summarize(&next),
                    });
                    current = next;
                }
                Err(reason) => {
                    let name = stage.name();
                    traces.push(StageTrace {
                        name,
                        ok: false,
                        detail: reason.clone(),
                    });
                    return Err(PipelineError {
                        stage: name,
                        index,
                        reason,
                        traces,
                    });
                }
            }
        }
        Ok(RunReport {
            output: current,
            traces,
        })
    }
}

/// Shape summary of a value: `object{3}`, `array[2]`, `string[7]`,
/// `number(1)`, `bool(true)`, `null`.
///
/// Deliberately *shape*, not content: trace `detail` must be stable across
/// runs and must not leak values into a report. String lengths are char
/// counts, never byte lengths, so a multi-byte value is not misreported.
pub fn summarize(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => format!("bool({b})"),
        Value::Number(n) => format!("number({n})"),
        Value::String(s) => format!("string[{}]", s.chars().count()),
        Value::Array(a) => format!("array[{}]", a.len()),
        Value::Object(o) => format!("object{{{}}}", o.len()),
    }
}

/// One-line-per-stage status plus a final output-shape line, e.g.
/// `load: ok object{1}`, `chunk: fail <reason>`, `output: object{2} keys=[a, b]`.
/// Ports cognee's `pipeline_status` observability without its runtime:
/// deterministic (declaration order, sorted object keys), shape-only (never
/// dumps values), and capped so a wide object cannot flood a report.
pub fn status_report(report: &RunReport) -> String {
    let mut lines: Vec<String> = Vec::with_capacity(report.traces.len() + 1);
    for t in &report.traces {
        lines.push(format!(
            "{}: {} {}",
            t.name,
            if t.ok { "ok" } else { "fail" },
            t.detail
        ));
    }
    lines.push(format!("output: {}", output_shape(&report.output)));
    lines.join("\n")
}

/// Shape of a run output: objects list their sorted keys, arrays list their
/// length, scalars reuse [`summarize`]. Capped at 200 chars (char count, not
/// bytes) with a `…(truncated)` marker so a report never embeds a full value.
fn output_shape(value: &Value) -> String {
    let shape = match value {
        Value::Object(o) => {
            let mut keys: Vec<&str> = o.keys().map(String::as_str).collect();
            keys.sort_unstable();
            format!("object{{{}}} keys=[{}]", o.len(), keys.join(", "))
        }
        Value::Array(a) => format!("array[{}]", a.len()),
        other => summarize(other),
    };
    truncate_chars(&shape, 200)
}

/// Char-prefix `s` to `limit` chars, appending a marker when truncated.
/// Char (not byte) counting so multi-byte text is not split mid-codepoint.
fn truncate_chars(s: &str, limit: usize) -> String {
    if s.chars().count() <= limit {
        return s.to_string();
    }
    let mut out: String = s.chars().take(limit).collect();
    out.push_str("…(truncated)");
    out
}

/// A validated request to forget one top-level object key. `reason` records
/// *why* the key must go (provenance for the trace), so a forget is
/// explainable after the fact. Construct via [`tombstone`], which rejects
/// blank keys/reasons; the fields stay public so a caller can still build
/// one literally, but [`apply_forget`] only strips on exact key match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForgetTombstone {
    pub key: String,
    pub reason: String,
}

/// Validate a forget request: both `key` and `reason` must be non-blank after
/// trimming. Fails closed — a blank key would match nothing (or the wrong
/// thing) and a blank reason would make the forget unexplainable.
pub fn tombstone(key: &str, reason: &str) -> Result<ForgetTombstone, String> {
    if key.trim().is_empty() {
        return Err(
            "cognee: forget tombstone needs a non-blank key — refusing to build a tombstone that matches nothing".to_string(),
        );
    }
    if reason.trim().is_empty() {
        return Err(
            "cognee: forget tombstone needs a non-blank reason — refusing to forget without an explainable cause".to_string(),
        );
    }
    Ok(ForgetTombstone {
        key: key.to_string(),
        reason: reason.to_string(),
    })
}

/// Strip top-level object keys named by `tombs` (exact match, in order) and
/// return the value. Strip-only: surviving keys keep their values untouched,
/// nested objects are never descended into, and non-objects pass through
/// unchanged. Never fails — an empty tombstone list is a no-op.
pub fn apply_forget(mut value: Value, tombs: &[ForgetTombstone]) -> Value {
    let Value::Object(obj) = &mut value else {
        return value;
    };
    for t in tombs {
        obj.remove(t.key.as_str());
    }
    value
}

/// Declared shape of one stage in a graph: what it reads and what it writes.
/// Keys are JSON object keys, not Rust fields — a stage spec is data so a
/// graph can be loaded from a file and validated without constructing the
/// stages.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StageSpec {
    pub name: String,
    pub consumes: Vec<String>,
    pub produces: Vec<String>,
}

/// Validate a stage graph offline. Rejects, in declaration order:
///
/// - a stage whose name is empty after trimming (an unnamed stage cannot be
///   named in a trace, so it is rejected rather than labelled with a
///   placeholder);
/// - a duplicate stage name (the error names it and both indices);
/// - a `consumes` key that no *earlier* stage produced — the error names the
///   consuming stage, the missing key, and every key available at that point.
///
/// `produces` becomes visible to *later* stages only, so a stage cannot
/// satisfy its own `consumes` and a forward reference is always an error.
/// An empty `consumes` is always valid: a source stage needs no input.
pub fn validate_pipeline(specs: &[StageSpec]) -> Result<(), String> {
    let mut available: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for (index, spec) in specs.iter().enumerate() {
        let name = spec.name.trim();
        if name.is_empty() {
            return Err(format!(
                "cognee: stage {index} has an empty name — every stage needs a \
                 non-empty name so traces and errors can name it"
            ));
        }
        if let Some(first) = seen.iter().position(|s| s == name) {
            return Err(format!(
                "cognee: duplicate stage name `{name}` at index {index} \
                 (first declared at index {first}) — stage names must be unique \
                 so a failure trace identifies exactly one stage"
            ));
        }
        for key in &spec.consumes {
            if !available.iter().any(|k| k == key) {
                return Err(format!(
                    "cognee: stage `{name}` (index {index}) consumes `{key}`, which no \
                     earlier stage produces — available at this point: {}",
                    list_keys(&available)
                ));
            }
        }
        seen.push(name.to_string());
        for key in &spec.produces {
            if !available.iter().any(|k| k == key) {
                available.push(key.clone());
            }
        }
    }
    Ok(())
}

/// Sorted, comma-separated key listing for error text (sorted so the same
/// graph always yields the same message), or `(none)` when empty.
fn list_keys(keys: &[String]) -> String {
    if keys.is_empty() {
        return "(none)".to_string();
    }
    let mut sorted: Vec<&str> = keys.iter().map(String::as_str).collect();
    sorted.sort_unstable();
    sorted.join(", ")
}

/// Built-in stage: require that the input object carries `key` with a
/// non-null value, and pass the input through untouched.
///
/// It *errors* rather than substituting a default: a missing key means the
/// upstream contract was violated, and inventing a value here would hide the
/// violation from whoever reads the trace.
pub struct RequireKey(pub &'static str);

impl Stage for RequireKey {
    fn name(&self) -> &'static str {
        "RequireKey"
    }

    fn run(&mut self, input: Value) -> Result<Value, String> {
        let Some(obj) = input.as_object() else {
            return Err(format!(
                "RequireKey(`{}`): expected a JSON object to read the key from, got {}",
                self.0,
                summarize(&input)
            ));
        };
        match obj.get(self.0) {
            None | Some(Value::Null) => Err(format!(
                "RequireKey(`{}`): input object has no non-null `{}` — an earlier \
                 stage must produce it (this stage never invents a value); \
                 present keys: {}",
                self.0,
                self.0,
                list_keys(&obj.keys().cloned().collect::<Vec<_>>())
            )),
            Some(_) => Ok(input),
        }
    }
}

/// Built-in stage: insert (or overwrite) `key` with `value` in the input
/// object. Requires an object input — there is nothing to set a key on
/// otherwise, and silently promoting a scalar to an object would change the
/// value's meaning.
pub struct SetKey {
    pub key: &'static str,
    pub value: Value,
}

impl Stage for SetKey {
    fn name(&self) -> &'static str {
        "SetKey"
    }

    fn run(&mut self, input: Value) -> Result<Value, String> {
        match input {
            Value::Object(mut obj) => {
                obj.insert(self.key.to_string(), self.value.clone());
                Ok(Value::Object(obj))
            }
            other => Err(format!(
                "SetKey(`{}`): expected a JSON object to set the key on, got {}",
                self.key,
                summarize(&other)
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// Test stage: records that it ran (in a shared counter) and passes the
    /// input through unchanged — the probe used to prove a later stage never
    /// runs after a halt.
    struct Probe {
        name: &'static str,
        runs: Arc<AtomicUsize>,
    }

    impl Stage for Probe {
        fn name(&self) -> &'static str {
            self.name
        }

        fn run(&mut self, input: Value) -> Result<Value, String> {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(input)
        }
    }

    /// Test stage: always fails with a fixed reason.
    struct Boom(&'static str, &'static str);

    impl Stage for Boom {
        fn name(&self) -> &'static str {
            self.0
        }

        fn run(&mut self, _input: Value) -> Result<Value, String> {
            Err(self.1.to_string())
        }
    }

    /// Test stage: independent of `SetKey`, used to prove non-object output
    /// flows through untouched.
    struct EmitScalar(Value);

    impl Stage for EmitScalar {
        fn name(&self) -> &'static str {
            "EmitScalar"
        }

        fn run(&mut self, _input: Value) -> Result<Value, String> {
            Ok(self.0.clone())
        }
    }

    fn probe(name: &'static str, runs: &Arc<AtomicUsize>) -> Box<dyn Stage> {
        Box::new(Probe {
            name,
            runs: Arc::clone(runs),
        })
    }

    #[test]
    fn traces_follow_declaration_order_and_output_threads_through() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut p = Pipeline::new();
        p.push(Box::new(SetKey {
            key: "a",
            value: json!(1),
        }));
        p.push(Box::new(SetKey {
            key: "b",
            value: json!(2),
        }));
        p.push(probe("tail", &runs));

        let report = p.run(json!({"seed": true})).unwrap();
        let names: Vec<&str> = report.traces.iter().map(|t| t.name).collect();
        assert_eq!(names, vec!["SetKey", "SetKey", "tail"]);
        assert!(report.traces.iter().all(|t| t.ok));
        // Declaration order is the run order: each later stage saw the
        // earlier stage's write, so both keys survive to the output.
        assert_eq!(report.output, json!({"seed": true, "a": 1, "b": 2}));
        assert_eq!(report.traces.len(), 3);
        assert_eq!(report.traces[2].detail, "object{3}");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn non_object_output_passes_through_unmodified() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut p = Pipeline::new();
        p.push(Box::new(SetKey {
            key: "a",
            value: json!(1),
        }));
        p.push(Box::new(EmitScalar(json!("scalar"))));
        p.push(probe("tail", &runs));

        let report = p.run(json!({})).unwrap();
        assert_eq!(report.output, json!("scalar"));
        assert_eq!(report.traces[1].detail, "string[6]");
        assert_eq!(runs.load(Ordering::SeqCst), 1, "later stage still ran");
    }

    #[test]
    fn failing_stage_halts_and_error_names_stage_and_index() {
        let runs = Arc::new(AtomicUsize::new(0));
        let mut p = Pipeline::new();
        p.push(probe("first", &runs));
        p.push(Box::new(Boom("explode", "reason: no `doc` key")));
        p.push(probe("after", &runs));

        let err = p.run(json!({"x": 1})).unwrap_err();
        assert_eq!(err.stage, "explode");
        assert_eq!(err.index, 1);
        assert!(err.reason.contains("no `doc` key"));
        // Halt proof: the stage after the failure never executed.
        assert_eq!(
            runs.load(Ordering::SeqCst),
            1,
            "the stage after the failure must not run"
        );
        // Traces so far: the succeeded stage, then the failure.
        assert_eq!(err.halted_at(), vec!["first", "explode"]);
        assert!(err.traces[0].ok);
        assert_eq!(err.traces[1].name, "explode");
        assert!(!err.traces[1].ok);
        assert_eq!(err.traces[1].detail, err.reason);
    }

    #[test]
    fn display_names_stage_index_and_reason() {
        let mut p = Pipeline::new();
        p.push(Box::new(Boom("explode", "empty `body`")));
        let err = p.run(json!(null)).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("explode"), "{text}");
        assert!(text.contains("index 0"), "{text}");
        assert!(text.contains("empty `body`"), "{text}");
        // It is a real std error, so `?`/`Box<dyn Error>` call sites work.
        let dyn_err: Box<dyn std::error::Error> = Box::new(err);
        assert!(dyn_err.to_string().contains("explode"));
    }

    #[test]
    fn describe_is_deterministic_and_empty_pipeline_says_empty() {
        let mut p = Pipeline::new();
        assert_eq!(p.describe(), "(empty)");
        assert!(p.is_empty());
        assert_eq!(p.len(), 0);
        p.push(Box::new(SetKey {
            key: "a",
            value: json!(1),
        }));
        p.push(Box::new(RequireKey("a")));
        assert_eq!(p.describe(), "SetKey → RequireKey");
        assert_eq!(p.describe(), p.describe(), "describe is repeatable");
        assert_eq!(p.len(), 2);
        assert!(!p.is_empty());
    }

    #[test]
    fn empty_pipeline_returns_input_unchanged() {
        let mut p = Pipeline::new();
        let input = json!({"anything": [1, 2, 3]});
        let report = p.run(input.clone()).unwrap();
        assert_eq!(report.output, input);
        assert!(report.traces.is_empty());
    }

    #[test]
    fn validate_pipeline_accepts_a_chain_of_producers_then_consumers() {
        let specs = vec![
            StageSpec {
                name: "load".into(),
                consumes: vec![],
                produces: vec!["doc".into()],
            },
            StageSpec {
                name: "chunk".into(),
                consumes: vec!["doc".into()],
                produces: vec!["chunks".into()],
            },
            StageSpec {
                name: "index".into(),
                consumes: vec!["chunks".into(), "doc".into()],
                produces: vec![],
            },
        ];
        assert_eq!(validate_pipeline(&specs), Ok(()));
        assert_eq!(validate_pipeline(&[]), Ok(()), "empty graph is valid");
    }

    #[test]
    fn validate_pipeline_rejects_duplicate_name() {
        let specs = vec![
            StageSpec {
                name: "load".into(),
                consumes: vec![],
                produces: vec![],
            },
            StageSpec {
                name: "load".into(),
                consumes: vec![],
                produces: vec![],
            },
        ];
        let err = validate_pipeline(&specs).unwrap_err();
        assert!(err.contains("duplicate stage name `load`"), "{err}");
        assert!(err.contains("index 1"), "{err}");
        assert!(err.contains("index 0"), "{err}");
    }

    #[test]
    fn validate_pipeline_rejects_empty_name() {
        let specs = vec![StageSpec {
            name: "  ".into(),
            consumes: vec![],
            produces: vec![],
        }];
        let err = validate_pipeline(&specs).unwrap_err();
        assert!(err.contains("empty name"), "{err}");
    }

    #[test]
    fn validate_pipeline_rejects_unknown_consumed_key_and_lists_available() {
        let specs = vec![
            StageSpec {
                name: "load".into(),
                consumes: vec![],
                produces: vec!["doc".into()],
            },
            StageSpec {
                name: "index".into(),
                consumes: vec!["chunks".into()],
                produces: vec![],
            },
        ];
        let err = validate_pipeline(&specs).unwrap_err();
        assert!(err.contains("`index`"), "{err}");
        assert!(err.contains("chunks"), "{err}");
        assert!(err.contains("doc"), "available keys listed: {err}");
    }

    #[test]
    fn validate_pipeline_rejects_a_stage_consuming_its_own_or_a_later_produce() {
        // Forward references are errors: `produces` is visible to later
        // stages only, so a self-consume is not satisfied either.
        let specs = vec![StageSpec {
            name: "self".into(),
            consumes: vec!["x".into()],
            produces: vec!["x".into()],
        }];
        let err = validate_pipeline(&specs).unwrap_err();
        assert!(err.contains("(none)"), "nothing available yet: {err}");
    }

    #[test]
    fn require_key_errors_on_missing_key_instead_of_inventing_one() {
        let mut p = Pipeline::new();
        p.push(Box::new(RequireKey("doc")));

        // Passes with a non-null value, unchanged.
        let ok = p.run(json!({"doc": "text", "other": 1})).unwrap();
        assert_eq!(ok.output, json!({"doc": "text", "other": 1}));

        // Missing key → error naming the key, no invented value.
        let err = p.run(json!({"other": 1})).unwrap_err();
        assert_eq!(err.stage, "RequireKey");
        assert!(err.reason.contains("`doc`"), "{}", err.reason);
        assert!(
            err.reason.contains("other"),
            "names present keys: {}",
            err.reason
        );

        // Explicit null is missing, not "present but empty".
        let null_err = p.run(json!({"doc": null})).unwrap_err();
        assert!(
            null_err.reason.contains("non-null `doc`"),
            "{}",
            null_err.reason
        );

        // Non-object input is rejected by the stage that needs an object.
        let scalar_err = p.run(json!(7)).unwrap_err();
        assert!(
            scalar_err.reason.contains("number(7)"),
            "{}",
            scalar_err.reason
        );
    }

    #[test]
    fn status_report_lists_stages_and_output_shape() {
        let mut p = Pipeline::new();
        p.push(Box::new(SetKey {
            key: "b",
            value: json!(1),
        }));
        p.push(Box::new(SetKey {
            key: "a",
            value: json!(2),
        }));
        let report = p.run(json!({})).unwrap();
        let text = status_report(&report);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(lines[0], "SetKey: ok object{1}", "{text}");
        assert_eq!(lines[1], "SetKey: ok object{2}", "{text}");
        // Sorted keys, shape only — no values leak.
        assert_eq!(lines[2], "output: object{2} keys=[a, b]", "{text}");
    }

    #[test]
    fn status_report_marks_fail_and_covers_scalar_and_array_output() {
        let report = RunReport {
            output: json!([1, 2, 3]),
            traces: vec![
                StageTrace {
                    name: "load",
                    ok: true,
                    detail: "object{1}".to_string(),
                },
                StageTrace {
                    name: "chunk",
                    ok: false,
                    detail: "missing `doc`".to_string(),
                },
            ],
        };
        let text = status_report(&report);
        assert_eq!(
            text, "load: ok object{1}\nchunk: fail missing `doc`\noutput: array[3]",
            "{text}"
        );
        let scalar = RunReport {
            output: json!("hi"),
            traces: vec![],
        };
        assert_eq!(status_report(&scalar), "output: string[2]");
    }

    #[test]
    fn status_report_truncates_wide_objects_with_marker() {
        let mut map = serde_json::Map::new();
        for i in 0..60 {
            map.insert(format!("very_long_key_name_{i:02}"), json!(i));
        }
        let report = RunReport {
            output: Value::Object(map),
            traces: vec![],
        };
        let text = status_report(&report);
        assert!(text.starts_with("output: object{60} keys=["), "{text}");
        assert!(text.contains("…(truncated)"), "{text}");
        assert!(
            text.chars().count() <= "output: ".len() + 200 + "…(truncated)".len(),
            "{text}"
        );
        // Full output never embedded: a value string must not appear.
        assert!(!text.contains("very_long_key_name_59, "), "{text}");
    }

    #[test]
    fn apply_forget_strips_exact_top_level_keys_only() {
        let tombs = vec![tombstone("secret", "gdpr request").unwrap()];
        let out = apply_forget(
            json!({"secret": 1, "Secret": 2, "secret2": 3, "keep": 4}),
            &tombs,
        );
        assert_eq!(out, json!({"Secret": 2, "secret2": 3, "keep": 4}));
        // Nested objects are never descended into (strip, never rewrite).
        let nested = apply_forget(json!({"outer": {"secret": 1}}), &tombs);
        assert_eq!(nested, json!({"outer": {"secret": 1}}));
        // Surviving values are untouched.
        let keep = apply_forget(json!({"keep": [1, 2]}), &tombs);
        assert_eq!(keep, json!({"keep": [1, 2]}));
        // Empty tombstone list is a no-op.
        let noop = apply_forget(json!({"a": 1}), &[]);
        assert_eq!(noop, json!({"a": 1}));
    }

    #[test]
    fn tombstone_rejects_blank_key_or_reason() {
        assert!(tombstone("", "why").is_err());
        assert!(tombstone("   ", "why").is_err());
        assert!(tombstone("k", "").is_err());
        assert!(tombstone("k", "  ").is_err());
        let t = tombstone("k", "why").unwrap();
        assert_eq!(
            t,
            ForgetTombstone {
                key: "k".to_string(),
                reason: "why".to_string(),
            }
        );
    }

    #[test]
    fn apply_forget_passes_non_objects_through() {
        let tombs = vec![tombstone("k", "why").unwrap()];
        assert_eq!(apply_forget(json!([1, 2]), &tombs), json!([1, 2]));
        assert_eq!(apply_forget(json!("s"), &tombs), json!("s"));
        assert_eq!(apply_forget(json!(null), &tombs), json!(null));
    }

    #[test]
    fn summarize_is_shape_only_and_counts_chars() {
        assert_eq!(summarize(&json!(null)), "null");
        assert_eq!(summarize(&json!(true)), "bool(true)");
        assert_eq!(summarize(&json!(1.5)), "number(1.5)");
        assert_eq!(summarize(&json!("héllo")), "string[5]");
        assert_eq!(summarize(&json!([1, 2])), "array[2]");
        assert_eq!(summarize(&json!({"a": 1})), "object{1}");
    }
}
