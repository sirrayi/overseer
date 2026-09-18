//! Eval-and-judge config patterns ported from the prompt batch (arsenal B2).
//!
//! Five ports, all pure validation/evaluation over JSON — the config formats
//! are JSON here because the engine has no YAML parser and will not grow one
//! for this (promptfoo and lm-eval both accept JSON; the YAML files are the
//! same trees). What is ported is *the contract*:
//!
//! - **promptfoo config gate** — validate a test config before it runs, then
//!   evaluate its asserts against an output.
//! - **promptfoo redteam config** — the plugin/strategy vocabulary and a
//!   bounded `numTests`.
//! - **deepeval threshold** — the all/any/mean comparison a threshold test
//!   makes over per-case scores.
//! - **dspy offline compile** — pick the candidate program with the best
//!   metric over a trainset, deterministically (this is the `evals/`-only
//!   half of dspy: selection, no runtime, no optimizer).
//! - **opik judges** — judge definitions (name/rubric/threshold) and the
//!   pass/fail verdict.
//! - **lm-eval-harness task spec keys** — required keys and the closed sets
//!   for `output_type`/`metric_list`, so a task file fails before it runs.
//!
//! `// DEFERRED(owner): running any of these (a promptfoo harness, a dspy
//! optimizer, a judge model, an lm-eval task run) — the engine validates and
//! scores; execution stays with the eval rig under `eval/`.`

use serde_json::Value;

// ── promptfoo config gate ────────────────────────────────────────────────

/// One assertion a promptfoo test can carry.
#[derive(Debug, Clone, PartialEq)]
pub enum Assert {
    Contains(String),
    NotContains(String),
    Equals(String),
    /// Output similarity (0..=1) at or above the threshold.
    Similar(f64),
    /// Cost in USD at or below the threshold.
    Cost(f64),
    /// A rubric for a judge model — validated, never executed here.
    LlmRubric(String),
}

impl Assert {
    /// The `type` token this assert parses from.
    pub const TYPES: [&'static str; 6] = [
        "contains",
        "not-contains",
        "equals",
        "similar",
        "cost",
        "llm-rubric",
    ];

    pub fn kind(&self) -> &'static str {
        match self {
            Assert::Contains(_) => "contains",
            Assert::NotContains(_) => "not-contains",
            Assert::Equals(_) => "equals",
            Assert::Similar(_) => "similar",
            Assert::Cost(_) => "cost",
            Assert::LlmRubric(_) => "llm-rubric",
        }
    }
}

/// A validated promptfoo config.
#[derive(Debug, Clone, PartialEq)]
pub struct PromptfooConfig {
    pub prompts: Vec<String>,
    pub asserts: Vec<Assert>,
}

/// The result of checking one assert against one output.
#[derive(Debug, Clone, PartialEq)]
pub struct AssertResult {
    pub kind: String,
    pub passed: bool,
    pub detail: String,
}

/// Validate a promptfoo config (the JSON form of `promptfooconfig.yaml`).
/// Errors name the offending field, so a broken config is fixed from the
/// message rather than by re-reading the schema.
pub fn validate_promptfoo(config: &Value) -> Result<PromptfooConfig, String> {
    let prompts = config
        .get("prompts")
        .and_then(Value::as_array)
        .ok_or_else(|| "promptfoo: `prompts` must be a non-empty array".to_string())?;
    let prompts: Vec<String> = prompts
        .iter()
        .map(|p| {
            p.as_str()
                .map(str::to_string)
                .ok_or_else(|| "promptfoo: every `prompts` entry must be a string".to_string())
        })
        .collect::<Result<_, _>>()?;
    if prompts.is_empty() {
        return Err("promptfoo: `prompts` is empty — a config with no prompt tests nothing".into());
    }
    let mut asserts = Vec::new();
    // Asserts may sit at the top level (`defaultTest`) or on a single test.
    for source in ["assert", "defaultTest"] {
        let Some(node) = config.get(source) else {
            continue;
        };
        let list = if source == "defaultTest" {
            node.get("assert").and_then(Value::as_array)
        } else {
            node.as_array()
        };
        let Some(list) = list else { continue };
        for (i, a) in list.iter().enumerate() {
            asserts.push(parse_assert(a).map_err(|e| format!("{source}[{i}]: {e}"))?);
        }
    }
    Ok(PromptfooConfig { prompts, asserts })
}

/// Parse one assert. Throws on an unknown `type` (a typo'd assertion is a
/// test that silently never fails — worse than a config error).
pub fn parse_assert(a: &Value) -> Result<Assert, String> {
    let kind = a
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| "assert is missing `type`".to_string())?;
    let value = || -> Result<&Value, String> {
        a.get("value")
            .ok_or_else(|| format!("`{kind}` assert needs a `value`"))
    };
    match kind {
        "contains" => Ok(Assert::Contains(
            value()?
                .as_str()
                .ok_or_else(|| "`contains` value must be a string".to_string())?
                .to_string(),
        )),
        "not-contains" => Ok(Assert::NotContains(
            value()?
                .as_str()
                .ok_or_else(|| "`not-contains` value must be a string".to_string())?
                .to_string(),
        )),
        "equals" => Ok(Assert::Equals(
            value()?
                .as_str()
                .ok_or_else(|| "`equals` value must be a string".to_string())?
                .to_string(),
        )),
        "similar" => Ok(Assert::Similar(threshold(value()?, "similar")?)),
        "cost" => Ok(Assert::Cost(threshold(value()?, "cost")?)),
        "llm-rubric" => Ok(Assert::LlmRubric(
            value()?
                .as_str()
                .ok_or_else(|| "`llm-rubric` value must be a string".to_string())?
                .to_string(),
        )),
        other => Err(format!(
            "unknown assert type `{other}` — want one of: {}",
            Assert::TYPES.join(", ")
        )),
    }
}

fn threshold(v: &Value, kind: &str) -> Result<f64, String> {
    let n = v
        .as_f64()
        .ok_or_else(|| format!("`{kind}` value must be a number"))?;
    if !n.is_finite() || n < 0.0 || n > 1.0 && kind == "similar" {
        return Err(format!("`{kind}` value {n} out of range — want 0..1"));
    }
    Ok(n)
}

/// Evaluate every assert of the config against one output. `similarity` is
/// supplied by the caller (an embedding score is not this module's job) and
/// `cost` is the run's spend for that case.
pub fn evaluate(
    config: &PromptfooConfig,
    output: &str,
    cost: f64,
    similarity: f64,
) -> Vec<AssertResult> {
    config
        .asserts
        .iter()
        .map(|a| {
            let (passed, detail) = match a {
                Assert::Contains(s) => (
                    output.contains(s.as_str()),
                    format!(
                        "output {}contain `{s}`",
                        if output.contains(s.as_str()) {
                            ""
                        } else {
                            "does not "
                        }
                    ),
                ),
                Assert::NotContains(s) => (
                    !output.contains(s.as_str()),
                    format!(
                        "output {}contain `{s}` (must not)",
                        if output.contains(s.as_str()) {
                            "does"
                        } else {
                            "does not"
                        }
                    ),
                ),
                Assert::Equals(s) => (
                    output.trim() == s.trim(),
                    format!(
                        "output {} `{s}`",
                        if output.trim() == s.trim() {
                            "equals"
                        } else {
                            "differs from"
                        }
                    ),
                ),
                Assert::Similar(t) => (
                    similarity >= *t,
                    format!("similarity {similarity:.3} vs threshold {t}"),
                ),
                Assert::Cost(t) => (cost <= *t, format!("cost {cost:.4} vs threshold {t}")),
                // A rubric needs a judge model; the gate reports it as
                // unevaluated rather than pretending it passed.
                Assert::LlmRubric(r) => (
                    false,
                    format!("needs a judge model (rubric: {})", truncate(r, 60)),
                ),
            };
            AssertResult {
                kind: a.kind().to_string(),
                passed,
                detail,
            }
        })
        .collect()
}

fn truncate(s: &str, cap: usize) -> String {
    s.chars().take(cap).collect()
}

// ── promptfoo redteam config ─────────────────────────────────────────────

/// A validated redteam config.
#[derive(Debug, Clone, PartialEq)]
pub struct Redteam {
    pub plugins: Vec<String>,
    pub strategies: Vec<String>,
    pub num_tests: u32,
}

/// promptfoo's redteam plugin vocabulary (the subset the gate accepts).
pub const REDTEAM_PLUGINS: [&str; 8] = [
    "harmful",
    "pii",
    "prompt-injection",
    "jailbreak",
    "hallucination",
    "contracts",
    "excessive-agency",
    "competitors",
];

/// promptfoo's redteam strategy vocabulary.
pub const REDTEAM_STRATEGIES: [&str; 5] = [
    "basic",
    "jailbreak",
    "prompt-injection",
    "multilingual",
    "base64",
];

/// Upper bound on generated tests — a runaway `numTests` is a spend bug, so
/// the gate refuses it rather than discovering it on the invoice.
pub const MAX_REDTEAM_TESTS: u32 = 10_000;

pub fn validate_redteam(config: &Value) -> Result<Redteam, String> {
    let plugins = string_list(config, "plugins", &REDTEAM_PLUGINS)?;
    let strategies = string_list(config, "strategies", &REDTEAM_STRATEGIES)?;
    if plugins.is_empty() {
        return Err(
            "redteam: `plugins` is empty — a run with no attack surface tests nothing".into(),
        );
    }
    let num_tests = match config.get("numTests") {
        None => 5,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| "redteam: `numTests` must be a whole number".to_string())?,
    };
    if num_tests == 0 {
        return Err("redteam: `numTests` must be at least 1".into());
    }
    if num_tests > u64::from(MAX_REDTEAM_TESTS) {
        return Err(format!(
            "redteam: `numTests` {num_tests} exceeds the cap {MAX_REDTEAM_TESTS}"
        ));
    }
    Ok(Redteam {
        plugins,
        strategies,
        num_tests: num_tests as u32,
    })
}

fn string_list(config: &Value, key: &str, known: &[&str]) -> Result<Vec<String>, String> {
    let Some(node) = config.get(key) else {
        return Ok(Vec::new());
    };
    let list = node
        .as_array()
        .ok_or_else(|| format!("redteam: `{key}` must be an array"))?;
    let mut out = Vec::with_capacity(list.len());
    for (i, v) in list.iter().enumerate() {
        let s = v
            .as_str()
            .ok_or_else(|| format!("redteam: `{key}[{i}]` must be a string"))?;
        if !known.contains(&s) {
            return Err(format!(
                "redteam: unknown {key} entry `{s}` — want one of: {}",
                known.join(", ")
            ));
        }
        out.push(s.to_string());
    }
    Ok(out)
}

// ── deepeval threshold ──────────────────────────────────────────────────

/// How a threshold test combines per-case scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThresholdMode {
    /// Every case must clear the threshold (the strict default).
    All,
    /// At least one case must clear it.
    Any,
    /// The mean must clear it.
    Mean,
}

/// The threshold verdict. An empty score list never passes: "no cases" must
/// not read as "all cases passed".
pub fn threshold_ok(scores: &[f64], threshold: f64, mode: ThresholdMode) -> bool {
    if scores.is_empty() || !threshold.is_finite() {
        return false;
    }
    let clear = |s: &f64| s.is_finite() && *s >= threshold;
    match mode {
        ThresholdMode::All => scores.iter().all(clear),
        ThresholdMode::Any => scores.iter().any(clear),
        ThresholdMode::Mean => {
            let finite: Vec<f64> = scores.iter().copied().filter(|s| s.is_finite()).collect();
            if finite.is_empty() {
                return false;
            }
            finite.iter().sum::<f64>() / finite.len() as f64 >= threshold
        }
    }
}

// ── dspy offline compile ────────────────────────────────────────────────

/// One trainset example (dspy's `Example`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Example {
    pub input: String,
    pub label: String,
}

/// A candidate program: instructions plus few-shot demonstrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub instructions: String,
    pub demos: Vec<(String, String)>,
}

impl Program {
    /// The prompt this program would send — the compiled artifact's text.
    pub fn render(&self, input: &str) -> String {
        let mut out = self.instructions.clone();
        for (x, y) in &self.demos {
            out.push_str(&format!("\n\nExample input: {x}\nExample output: {y}"));
        }
        out.push_str(&format!("\n\nInput: {input}\nOutput:"));
        out
    }
}

/// A metric over (prediction, label) — 0..=1 like dspy's.
pub type Metric = fn(&str, &str) -> f64;

/// The compile result: the winning program, its trainset score, and the
/// per-example scores (which is what tells a human *why* it won).
#[derive(Debug, Clone, PartialEq)]
pub struct Compiled {
    pub program: Program,
    pub score: f64,
    pub per_example: Vec<f64>,
}

/// Offline compile (dspy's selection half, no optimizer, no runtime): score
/// every candidate over the trainset with `metric` and keep the best.
/// `predict` is injected so the compile is offline and deterministic — the
/// harness supplies whatever predictor the arm under test uses (a lookup
/// table in tests, a provider call in a real run).
///
/// Ties keep the earlier candidate, so a candidate list is a *priority
/// order* and the result is reproducible. An empty trainset or an empty
/// candidate list yields `None` — there is nothing to select on.
pub fn compile(
    trainset: &[Example],
    candidates: &[Program],
    metric: Metric,
    predict: impl Fn(&Program, &str) -> String,
) -> Option<Compiled> {
    if trainset.is_empty() || candidates.is_empty() {
        return None;
    }
    let mut best: Option<Compiled> = None;
    for program in candidates {
        let per_example: Vec<f64> = trainset
            .iter()
            .map(|ex| {
                let pred = predict(program, &ex.input);
                let s = metric(&pred, &ex.label);
                if s.is_finite() {
                    s
                } else {
                    0.0
                }
            })
            .collect();
        let score = per_example.iter().sum::<f64>() / per_example.len() as f64;
        let better = match &best {
            None => true,
            Some(b) => score > b.score,
        };
        if better {
            best = Some(Compiled {
                program: program.clone(),
                score,
                per_example,
            });
        }
    }
    best
}

// ── opik judges ─────────────────────────────────────────────────────────

/// One judge definition (the `judges.py` shape: a name, a rubric, a pass
/// threshold).
#[derive(Debug, Clone, PartialEq)]
pub struct Judge {
    pub name: String,
    pub rubric: String,
    pub threshold: f64,
}

/// Parse judge definitions from JSON. Errors name the offending judge.
pub fn parse_judges(json: &Value) -> Result<Vec<Judge>, String> {
    let list = json
        .as_array()
        .ok_or_else(|| "opik: judges must be an array of definitions".to_string())?;
    let mut out = Vec::with_capacity(list.len());
    for (i, j) in list.iter().enumerate() {
        let name = j
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("opik: judge {i} needs a non-empty `name`"))?
            .to_string();
        let rubric = j
            .get("rubric")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| format!("opik: judge `{name}` needs a non-empty `rubric`"))?
            .to_string();
        let threshold = match j.get("threshold") {
            None => 0.5,
            Some(v) => v
                .as_f64()
                .ok_or_else(|| format!("opik: judge `{name}` has a non-numeric threshold"))?,
        };
        if !threshold.is_finite() || threshold < 0.0 || threshold > 1.0 {
            return Err(format!(
                "opik: judge `{name}` threshold {threshold} out of range — want 0..1"
            ));
        }
        out.push(Judge {
            name,
            rubric,
            threshold,
        });
    }
    Ok(out)
}

/// Whether a judge's score clears its threshold. A non-finite score never
/// passes (an unparsable judge reply must not read as a pass).
pub fn judge_passes(judge: &Judge, score: f64) -> bool {
    score.is_finite() && score >= judge.threshold
}

// ── lm-eval-harness task spec ───────────────────────────────────────────

/// lm-eval-harness's closed `output_type` set.
pub const LM_OUTPUT_TYPES: [&str; 4] = [
    "multiple_choice",
    "generate_until",
    "loglikelihood",
    "loglikelihood_rolling",
];

/// The metric names the gate accepts.
pub const LM_METRICS: [&str; 6] = ["exact_match", "acc", "acc_norm", "f1", "bleu", "perplexity"];

/// The validated keys of a task spec (`task.yaml`/JSON): the ones that
/// decide whether the task can run at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskSpec {
    pub doc_to_text: String,
    pub doc_to_target: String,
    pub output_type: String,
    pub metric_list: Vec<String>,
    pub num_fewshot: u32,
}

/// Validate a task spec. Required: `doc_to_text`, `doc_to_target`,
/// `output_type` (closed set), and a non-empty `metric_list` (closed set).
/// `num_fewshot` defaults to 0.
pub fn validate_taskspec(spec: &Value) -> Result<TaskSpec, String> {
    let req = |key: &str| -> Result<String, String> {
        spec.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| format!("lm-eval: task spec is missing `{key}`"))
    };
    let doc_to_text = req("doc_to_text")?;
    let doc_to_target = req("doc_to_target")?;
    let output_type = req("output_type")?;
    if !LM_OUTPUT_TYPES.contains(&output_type.as_str()) {
        return Err(format!(
            "lm-eval: unknown output_type `{output_type}` — want one of: {}",
            LM_OUTPUT_TYPES.join(", ")
        ));
    }
    let metrics = spec
        .get("metric_list")
        .and_then(Value::as_array)
        .ok_or_else(|| "lm-eval: task spec is missing `metric_list`".to_string())?;
    let mut metric_list = Vec::with_capacity(metrics.len());
    for (i, m) in metrics.iter().enumerate() {
        // A metric entry is either a name or `{metric: name, …}`.
        let name = m
            .as_str()
            .or_else(|| m.get("metric").and_then(Value::as_str))
            .ok_or_else(|| format!("lm-eval: metric_list[{i}] names no metric"))?;
        if !LM_METRICS.contains(&name) {
            return Err(format!(
                "lm-eval: unknown metric `{name}` — want one of: {}",
                LM_METRICS.join(", ")
            ));
        }
        metric_list.push(name.to_string());
    }
    if metric_list.is_empty() {
        return Err("lm-eval: `metric_list` is empty — nothing would be scored".into());
    }
    let num_fewshot = match spec.get("num_fewshot") {
        None => 0,
        Some(v) => v
            .as_u64()
            .ok_or_else(|| "lm-eval: `num_fewshot` must be a whole number".to_string())?,
    };
    Ok(TaskSpec {
        doc_to_text,
        doc_to_target,
        output_type,
        metric_list,
        num_fewshot: num_fewshot as u32,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn promptfoo_gate_validates_then_evaluates() {
        let cfg = json!({
            "prompts": ["Answer briefly: {{q}}"],
            "assert": [
                {"type": "contains", "value": "42"},
                {"type": "not-contains", "value": "sorry"},
                {"type": "similar", "value": 0.8},
                {"type": "cost", "value": 0.01},
                {"type": "llm-rubric", "value": "is the tone helpful?"}
            ]
        });
        let parsed = validate_promptfoo(&cfg).unwrap();
        assert_eq!(parsed.prompts.len(), 1);
        assert_eq!(parsed.asserts.len(), 5);
        let results = evaluate(&parsed, "the answer is 42", 0.004, 0.91);
        let by = |k: &str| {
            results
                .iter()
                .find(|r| r.kind == k)
                .unwrap_or_else(|| panic!("no {k} result"))
        };
        assert!(by("contains").passed);
        assert!(by("not-contains").passed, "{}", by("not-contains").detail);
        assert!(by("similar").passed);
        assert!(by("cost").passed);
        // A rubric is never silently "passed" without a judge.
        assert!(!by("llm-rubric").passed);
        assert!(by("llm-rubric").detail.contains("judge model"));

        // A failing case reports why.
        let bad = evaluate(&parsed, "sorry, no idea", 0.5, 0.1);
        assert!(!bad.iter().find(|r| r.kind == "contains").unwrap().passed);
        assert!(
            !bad.iter()
                .find(|r| r.kind == "not-contains")
                .unwrap()
                .passed
        );
        assert!(!bad.iter().find(|r| r.kind == "cost").unwrap().passed);
        assert!(bad
            .iter()
            .find(|r| r.kind == "similar")
            .unwrap()
            .detail
            .contains("0.100"));
    }

    #[test]
    fn promptfoo_gate_rejects_incomplete_or_unknown_configs() {
        // Empty prompts, unknown assert type, missing value, out-of-range
        // threshold — each names what is wrong.
        let err = validate_promptfoo(&json!({"prompts": []})).unwrap_err();
        assert!(err.contains("empty"), "{err}");
        assert!(validate_promptfoo(&json!({}))
            .unwrap_err()
            .contains("`prompts` must be a non-empty array"));
        assert!(validate_promptfoo(&json!({"prompts": [1]}))
            .unwrap_err()
            .contains("must be a string"));
        let err = validate_promptfoo(&json!({
            "prompts": ["p"],
            "assert": [{"type": "vibes", "value": "x"}]
        }))
        .unwrap_err();
        assert!(err.contains("vibes"), "{err}");
        assert!(err.contains("contains"), "lists the vocabulary: {err}");
        assert!(validate_promptfoo(&json!({
            "prompts": ["p"],
            "assert": [{"type": "contains"}]
        }))
        .unwrap_err()
        .contains("needs a `value`"));
        assert!(validate_promptfoo(&json!({
            "prompts": ["p"],
            "assert": [{"type": "similar", "value": 1.5}]
        }))
        .unwrap_err()
        .contains("out of range"));
        // A config with no asserts is valid (a smoke config).
        assert!(validate_promptfoo(&json!({"prompts": ["p"]}))
            .unwrap()
            .asserts
            .is_empty());
    }

    #[test]
    fn redteam_vocabulary_and_spend_bound_are_enforced() {
        let ok = validate_redteam(&json!({
            "plugins": ["pii", "prompt-injection"],
            "strategies": ["jailbreak"],
            "numTests": 10
        }))
        .unwrap();
        assert_eq!(ok.plugins.len(), 2);
        assert_eq!(ok.strategies, vec!["jailbreak".to_string()]);
        assert_eq!(ok.num_tests, 10);
        // Defaults: no strategies, 5 tests.
        let d = validate_redteam(&json!({"plugins": ["pii"]})).unwrap();
        assert!(d.strategies.is_empty());
        assert_eq!(d.num_tests, 5);
        // Unknown vocabulary, empty plugin list, and the spend bound.
        assert!(validate_redteam(&json!({"plugins": ["mind-reading"]}))
            .unwrap_err()
            .contains("mind-reading"));
        assert!(validate_redteam(&json!({"plugins": []}))
            .unwrap_err()
            .contains("empty"));
        let err = validate_redteam(&json!({
            "plugins": ["pii"],
            "numTests": MAX_REDTEAM_TESTS + 1
        }))
        .unwrap_err();
        assert!(err.contains("exceeds the cap"), "{err}");
        assert!(
            validate_redteam(&json!({"plugins": ["pii"], "numTests": 0}))
                .unwrap_err()
                .contains("at least 1")
        );
        assert!(
            validate_redteam(&json!({"plugins": ["pii"], "strategies": "jailbreak"}))
                .unwrap_err()
                .contains("must be an array")
        );
    }

    #[test]
    fn deepeval_threshold_modes() {
        let scores = [0.9, 0.4, 0.8];
        assert!(!threshold_ok(&scores, 0.5, ThresholdMode::All));
        assert!(threshold_ok(&scores, 0.5, ThresholdMode::Any));
        assert!(threshold_ok(&scores, 0.7, ThresholdMode::Mean), "mean 0.7");
        assert!(!threshold_ok(&scores, 0.71, ThresholdMode::Mean));
        // Boundary is inclusive.
        assert!(threshold_ok(&[0.5], 0.5, ThresholdMode::All));
        // No cases never passes, whatever the mode.
        assert!(!threshold_ok(&[], 0.0, ThresholdMode::All));
        assert!(!threshold_ok(&[], 0.0, ThresholdMode::Any));
        assert!(!threshold_ok(&[], 0.0, ThresholdMode::Mean));
        // A non-finite score cannot clear a threshold; the mean ignores it.
        assert!(!threshold_ok(&[f64::NAN], 0.0, ThresholdMode::All));
        assert!(!threshold_ok(&[f64::INFINITY], 0.0, ThresholdMode::Any));
        assert!(threshold_ok(&[1.0, f64::NAN], 1.0, ThresholdMode::Mean));
        assert!(!threshold_ok(&scores, f64::NAN, ThresholdMode::Any));
    }

    #[test]
    fn dspy_compile_selects_by_trainset_metric_deterministically() {
        let trainset = vec![
            Example {
                input: "2+2".into(),
                label: "4".into(),
            },
            Example {
                input: "3+3".into(),
                label: "6".into(),
            },
        ];
        let candidates = vec![
            Program {
                instructions: "answer with a number".into(),
                demos: vec![],
            },
            Program {
                instructions: "answer with a number".into(),
                demos: vec![("2+2".into(), "4".into())],
            },
        ];
        // The oracle predictor: a demo lookup, else a wrong guess.
        let predict = |p: &Program, input: &str| -> String {
            p.demos
                .iter()
                .find(|(x, _)| x == input)
                .map(|(_, y)| y.clone())
                .unwrap_or_else(|| "0".into())
        };
        let exact: Metric = |pred, label| if pred == label { 1.0 } else { 0.0 };
        let compiled = compile(&trainset, &candidates, exact, predict).unwrap();
        assert_eq!(
            compiled.program.demos.len(),
            1,
            "the demo-bearing program wins"
        );
        assert!((compiled.score - 0.5).abs() < 1e-9, "one of two examples");
        assert_eq!(compiled.per_example, vec![1.0, 0.0]);
        // The rendered artifact carries instructions, demos, and the input.
        let rendered = compiled.program.render("5+5");
        assert!(rendered.contains("answer with a number"));
        assert!(rendered.contains("Example input: 2+2"));
        assert!(rendered.ends_with("Input: 5+5\nOutput:"));

        // Deterministic tie-break: equal scores keep the earlier candidate.
        let tie = compile(&trainset, &candidates, |_, _| 0.5, |_, _| String::new()).unwrap();
        assert!(tie.program.demos.is_empty(), "first declared wins a tie");
        // Nothing to select on.
        assert!(compile(&[], &candidates, exact, predict).is_none());
        assert!(compile(&trainset, &[], exact, predict).is_none());
        // A non-finite metric score cannot poison the mean.
        let nan = compile(&trainset, &candidates, |_, _| f64::NAN, predict).unwrap();
        assert_eq!(nan.score, 0.0);
    }

    #[test]
    fn opik_judges_parse_and_verdict() {
        let judges = parse_judges(&json!([
            {"name": "helpfulness", "rubric": "Is the answer helpful?", "threshold": 0.8},
            {"name": "safety", "rubric": "No harmful content"}
        ]))
        .unwrap();
        assert_eq!(judges.len(), 2);
        assert_eq!(judges[0].threshold, 0.8);
        assert_eq!(judges[1].threshold, 0.5, "default threshold");
        assert!(judge_passes(&judges[0], 0.8), "inclusive");
        assert!(!judge_passes(&judges[0], 0.79));
        assert!(
            !judge_passes(&judges[0], f64::NAN),
            "a NaN score never passes"
        );
        assert!(judge_passes(&judges[1], 1.0));
        // Malformed definitions name the judge.
        assert!(parse_judges(&json!({})).unwrap_err().contains("array"));
        assert!(parse_judges(&json!([{"rubric": "r"}]))
            .unwrap_err()
            .contains("judge 0"));
        assert!(
            parse_judges(&json!([{"name": "a", "rubric": "r", "threshold": 2.0}]))
                .unwrap_err()
                .contains("out of range")
        );
        assert!(parse_judges(&json!([{"name": "a"}]))
            .unwrap_err()
            .contains("rubric"));
    }

    #[test]
    fn lm_eval_task_spec_keys_are_required_and_closed() {
        let spec = validate_taskspec(&json!({
            "doc_to_text": "Q: {{question}}",
            "doc_to_target": "{{answer}}",
            "output_type": "multiple_choice",
            "metric_list": ["acc", {"metric": "exact_match"}],
            "num_fewshot": 5
        }))
        .unwrap();
        assert_eq!(spec.output_type, "multiple_choice");
        assert_eq!(spec.metric_list, vec!["acc", "exact_match"]);
        assert_eq!(spec.num_fewshot, 5);
        // Defaults.
        let d = validate_taskspec(&json!({
            "doc_to_text": "a",
            "doc_to_target": "b",
            "output_type": "generate_until",
            "metric_list": ["bleu"]
        }))
        .unwrap();
        assert_eq!(d.num_fewshot, 0);
        // Missing keys are named one by one.
        for key in ["doc_to_text", "doc_to_target", "output_type"] {
            let mut v = json!({
                "doc_to_text": "a", "doc_to_target": "b",
                "output_type": "generate_until", "metric_list": ["acc"]
            });
            v.as_object_mut().unwrap().remove(key);
            let err = validate_taskspec(&v).unwrap_err();
            assert!(err.contains(key), "expected {key} in {err}");
        }
        // Closed vocabularies.
        let err = validate_taskspec(&json!({
            "doc_to_text": "a", "doc_to_target": "b",
            "output_type": "vibes", "metric_list": ["acc"]
        }))
        .unwrap_err();
        assert!(
            err.contains("vibes") && err.contains("generate_until"),
            "{err}"
        );
        assert!(validate_taskspec(&json!({
            "doc_to_text": "a", "doc_to_target": "b",
            "output_type": "generate_until", "metric_list": ["vibes"]
        }))
        .unwrap_err()
        .contains("vibes"));
        assert!(validate_taskspec(&json!({
            "doc_to_text": "a", "doc_to_target": "b",
            "output_type": "generate_until", "metric_list": []
        }))
        .unwrap_err()
        .contains("empty"));
        assert!(validate_taskspec(&json!({
            "doc_to_text": "a", "doc_to_target": "b",
            "output_type": "generate_until", "metric_list": ["acc"], "num_fewshot": "five"
        }))
        .unwrap_err()
        .contains("whole number"));
    }
}
