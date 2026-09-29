//! `struct_search` tool — structural code search (ast-grep) plus a
//! rules-based audit lane (semgrep). Two ports, one tool (arsenal B2):
//!
//! - **ast-grep `run --json`** — the search lane. `pattern` is *code*, not a
//!   regex: `$A` / `$$$ARGS` metavariables match any node, so whitespace,
//!   comments and formatting are irrelevant. Matches come back sorted by
//!   (file, line, column) so the same probe answers identically twice.
//! - **semgrep `--json`** — the audit lane. Rules from a LOCAL `--config`
//!   file or directory run against `path`; every finding carries severity,
//!   check id and message, sorted by (file, line, check). The lane is
//!   classified Read, so it must not egress: `--metrics=off` is always
//!   passed, and `auto`, registry ids (`p/…`, `r/…`, `s/…`) and URLs are
//!   refused before anything spawns.
//!
//! Backends are opt-in helper binaries — never linked, never bundled:
//!
//! | lane       | variable             | fallback                         |
//! |------------|----------------------|----------------------------------|
//! | `ast-grep` | `OVERSEER_AST_GREP`  | `ast-grep`, then `sg`, on `PATH` |
//! | `semgrep`  | `OVERSEER_SEMGREP`   | `semgrep` on `PATH`              |
//!
//! A variable naming a file that is not there counts as *unconfigured* — a
//! stale `OVERSEER_AST_GREP` must never look like a working backend (same
//! rule as `computer`). The child gets the allowlisted environment from
//! `bash`, so a probe never inherits the operator's secrets.
//!
//! Fail-open is the contract (same as `diagnostics`): a missing binary, a
//! spawn failure, a timeout, output past the probe cap, an empty/non-zero
//! exit, an unparseable stream — each is a plain *note* naming the lane, what
//! happened and the variable that configures the binary. Never a tool error,
//! never a panic: a probe must not be the reason a turn dies. The only hard
//! errors are the caller's own mistakes (missing/empty `pattern` on the
//! ast-grep lane, a missing or non-local `config` on the semgrep lane, an
//! unknown `lane`). A non-zero exit *with* parseable rows is a normal result —
//! semgrep exits non-zero when it found something — so the exit code is
//! evidence, not a verdict.
//!
//! Position discipline: ast-grep reports 0-based line/column, semgrep 1-based.
//! Both are reported 1-based here, matching `read` and `diagnostics`, so a
//! hit can be pasted into `read path:line` unchanged.
//!
//! `// DEFERRED(owner): a tree-sitter in-process backend (the feature-gated
//! `crate::tsitter` module is its registry — this port deliberately does not
//! import it, so the tool builds without the optional parser stack);
//! incremental re-indexing (every probe re-walks `path` from scratch);
//! semgrep's own rule registry (rules arrive via a local `--config` only, no
//! registry fetch or cache) — all P8-C+ material.`

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{need_str, opt_u64, schema, ToolCtx, ToolOutput};

/// Variable naming the ast-grep binary for the search lane.
pub const ENV_AST_GREP: &str = "OVERSEER_AST_GREP";
/// Variable naming the semgrep binary for the audit lane.
pub const ENV_SEMGREP: &str = "OVERSEER_SEMGREP";

/// Binary names looked up on `PATH` for the search lane, in this order
/// (ast-grep ships `ast-grep` with an `sg` alias). A constant order keeps
/// detection deterministic — never a directory-listing order.
const AST_GREP_NAMES: [&str; 2] = ["ast-grep", "sg"];

/// Binary name looked up on `PATH` for the audit lane.
const SEMGREP_NAMES: [&str; 1] = ["semgrep"];

/// Wall-clock cap for one probe; a runaway scan never blocks a turn.
const PROBE_TIMEOUT_S: u64 = 120;

/// Grace period for collecting a reader's buffer once the child is gone. The
/// pipes are normally at EOF by then; a grandchild that still holds one must
/// not hold the turn hostage.
const PIPE_GRACE: Duration = Duration::from_secs(5);

/// Model-facing row cap, per lane. `limit` can only narrow it.
const MAX_ROWS: usize = 50;

/// Cap on the stdout we will accept as one probe's result. Past this the
/// answer is a note, not a parse of a hundred megabytes of JSON.
const MAX_STDOUT: usize = 4_000_000;

/// Characters of a backend's stderr echoed into a note (char-based, so a
/// multi-byte char is never split).
const STDERR_EXCERPT: usize = 300;

/// Minimal child environment, the same rule as `tools/bash.rs`: a probe gets
/// what a toolchain needs to run and nothing else. Secrets stay out of the
/// child's scope unless deliberately inherited (playbook Ch.10 §4).
const ENV_ALLOW: [&str; 8] = [
    "PATH", "HOME", "LANG", "LC_ALL", "TMPDIR", "TERM", "USER", "SHELL",
];

/// Which backend answers the probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lane {
    /// ast-grep structural search (`run --pattern … --json`).
    AstGrep,
    /// semgrep rules-based audit scan (`--json --quiet --config …`).
    Semgrep,
}

impl Lane {
    /// Every lane, in the order the description lists them.
    pub const ALL: [Lane; 2] = [Lane::AstGrep, Lane::Semgrep];

    /// The lane's wire name — exactly the value accepted in `lane` and the
    /// name every note and row uses.
    pub fn as_str(self) -> &'static str {
        match self {
            Lane::AstGrep => "ast-grep",
            Lane::Semgrep => "semgrep",
        }
    }

    /// The lane's canonical name, case- and whitespace-insensitively.
    /// `Err` lists both valid lanes instead of guessing at a typo.
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ast-grep" => Ok(Lane::AstGrep),
            "semgrep" => Ok(Lane::Semgrep),
            other => Err(format!(
                "unknown lane `{other}` — valid lanes: ast-grep, semgrep."
            )),
        }
    }

    /// The variable that configures this lane's binary. Named in every
    /// fail-open note so the repair is one env var away.
    pub fn env(self) -> &'static str {
        match self {
            Lane::AstGrep => ENV_AST_GREP,
            Lane::Semgrep => ENV_SEMGREP,
        }
    }

    /// Binary names tried on `PATH` when the variable is unset.
    fn path_names(self) -> &'static [&'static str] {
        match self {
            Lane::AstGrep => &AST_GREP_NAMES,
            Lane::Semgrep => &SEMGREP_NAMES,
        }
    }

    /// Human phrase for the backend's output shape, used by field errors.
    fn output_hint(self) -> &'static str {
        match self {
            Lane::AstGrep => "ast-grep `run --pattern … --json` output",
            Lane::Semgrep => "semgrep `--json` output",
        }
    }
}

/// The two backends this tool may shell out to. `None` is the shipped
/// default: structural search is opt-in, so a stock install runs no external
/// scanner.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Bins {
    pub ast_grep: Option<PathBuf>,
    pub semgrep: Option<PathBuf>,
}

impl Bins {
    /// Read the operator's configuration: the env override first (a variable
    /// naming a non-existent file counts as unset), else the binary's name on
    /// `PATH`.
    pub fn detect() -> Self {
        Bins {
            ast_grep: from_env_value(std::env::var(ENV_AST_GREP).ok().as_deref())
                .or_else(|| find_on_path(Lane::AstGrep.path_names())),
            semgrep: from_env_value(std::env::var(ENV_SEMGREP).ok().as_deref())
                .or_else(|| find_on_path(Lane::Semgrep.path_names())),
        }
    }

    /// The binary configured for `lane`, or `None` while it is unconfigured.
    pub fn binary(&self, lane: Lane) -> Option<&Path> {
        match lane {
            Lane::AstGrep => self.ast_grep.as_deref(),
            Lane::Semgrep => self.semgrep.as_deref(),
        }
    }
}

/// One structural match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hit {
    pub file: String,
    pub line: u64,
    pub column: u64,
    /// The matched code, flattened to its first line.
    pub text: String,
}

/// One audit finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Audit {
    pub file: String,
    pub line: u64,
    /// Upper-cased so `ERROR`/`WARNING`/`INFO` compare across semgrep
    /// versions; `UNKNOWN` when the backend omitted it (never a quiet
    /// downgrade to `INFO`).
    pub severity: String,
    pub check: String,
    pub message: String,
}

/// Parse `ast-grep run --pattern … --json` stdout into hits sorted by
/// (file, line, column).
///
/// Invariants: ast-grep's 0-based positions are reported 1-based; a
/// multi-line match is flattened to its first line plus ` …`; a match
/// missing `file` or `range.start.{line,column}` is an error naming the field
/// (that is not ast-grep output, and inventing a location would be worse than
/// saying so).
pub fn parse_astgrep_json(stdout: &str) -> Result<Vec<Hit>, String> {
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "ast-grep: stdout is not JSON ({e}) — expected the array `ast-grep run \
             --pattern <pattern> --json <path>` prints.",
        )
    })?;
    let matches = value.as_array().ok_or_else(|| {
        format!(
            "ast-grep: expected a JSON array of matches, got {} — plain `--json` prints an array; \
             `--json=stream` prints one object per line and this lane does not read that.",
            kind_of(&value)
        )
    })?;
    let mut hits = Vec::with_capacity(matches.len());
    for (i, m) in matches.iter().enumerate() {
        let file = m
            .get("file")
            .and_then(Value::as_str)
            .ok_or_else(|| missing_field(Lane::AstGrep, i, "file"))?;
        let start = m
            .pointer("/range/start")
            .ok_or_else(|| missing_field(Lane::AstGrep, i, "range.start"))?;
        let line = start
            .get("line")
            .and_then(Value::as_u64)
            .ok_or_else(|| missing_field(Lane::AstGrep, i, "range.start.line"))?;
        let column = start
            .get("column")
            .and_then(Value::as_u64)
            .ok_or_else(|| missing_field(Lane::AstGrep, i, "range.start.column"))?;
        let text = m
            .get("text")
            .and_then(Value::as_str)
            .or_else(|| m.get("lines").and_then(Value::as_str))
            .unwrap_or_default();
        hits.push(Hit {
            file: file.to_string(),
            // ast-grep counts lines and columns from 0; we report 1-based.
            line: line.saturating_add(1),
            column: column.saturating_add(1),
            text: first_line(text),
        });
    }
    hits.sort_by(|a, b| (&a.file, a.line, a.column).cmp(&(&b.file, b.line, b.column)));
    Ok(hits)
}

/// Parse semgrep `--json` stdout into findings sorted by (file, line, check).
///
/// Invariants: semgrep's 1-based line numbers are reported as-is; a run that
/// reports only `errors` (no `results` key at all — the shape semgrep emits
/// when every rule failed to load) is an *empty* audit list, not a failure,
/// because "nothing was reported" is a real answer; an absent severity is
/// `UNKNOWN`; a finding missing `path`, `start.line` or `check_id` is an
/// error naming the field.
pub fn parse_semgrep_json(stdout: &str) -> Result<Vec<Audit>, String> {
    let value: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "semgrep: stdout is not JSON ({e}) — expected the object `semgrep --json --quiet …` prints.",
        )
    })?;
    let obj = value.as_object().ok_or_else(|| {
        format!(
            "semgrep: expected a JSON object with a `results` array, got {}.",
            kind_of(&value)
        )
    })?;
    let results = match obj.get("results") {
        // No `results` key (errors-only output) or an explicit null: nothing
        // was found. Not an error — an empty audit lane is a valid answer.
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(r) => r.as_array().ok_or_else(|| {
            format!(
                "semgrep: `results` must be an array of findings, got {}.",
                kind_of(r)
            )
        })?,
    };
    let mut audits = Vec::with_capacity(results.len());
    for (i, r) in results.iter().enumerate() {
        let file = r
            .get("path")
            .and_then(Value::as_str)
            .ok_or_else(|| missing_field(Lane::Semgrep, i, "path"))?;
        let line = r
            .pointer("/start/line")
            .and_then(Value::as_u64)
            .ok_or_else(|| missing_field(Lane::Semgrep, i, "start.line"))?;
        let check = r
            .get("check_id")
            .and_then(Value::as_str)
            .ok_or_else(|| missing_field(Lane::Semgrep, i, "check_id"))?;
        let severity = r
            .pointer("/extra/severity")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .trim()
            .to_ascii_uppercase();
        let message = r
            .pointer("/extra/message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        audits.push(Audit {
            file: file.to_string(),
            line,
            severity,
            check: check.to_string(),
            message: first_line(message),
        });
    }
    audits.sort_by(|a, b| (&a.file, a.line, &a.check).cmp(&(&b.file, b.line, &b.check)));
    Ok(audits)
}

/// Render hits as one `file:line:col: text` line each, capped at [`MAX_ROWS`]
/// with an explicit `… N more matches truncated` line (never a silent drop).
pub fn render_hits(hits: &[Hit]) -> String {
    let shown = hits.len().min(MAX_ROWS);
    let mut out = String::new();
    for h in &hits[..shown] {
        out.push_str(&format!("{}:{}:{}: {}\n", h.file, h.line, h.column, h.text));
    }
    if hits.len() > shown {
        out.push_str(&format!(
            "… {} more matches truncated\n",
            hits.len() - shown
        ));
    }
    out.trim_end().to_string()
}

/// Render findings as one `file:line: SEVERITY check: message` line each,
/// capped at [`MAX_ROWS`] with an explicit `… N more findings truncated` line.
pub fn render_audits(audits: &[Audit]) -> String {
    let shown = audits.len().min(MAX_ROWS);
    let mut out = String::new();
    for a in &audits[..shown] {
        out.push_str(&format!(
            "{}:{}: {} {}: {}\n",
            a.file, a.line, a.severity, a.check, a.message
        ));
    }
    if audits.len() > shown {
        out.push_str(&format!(
            "… {} more findings truncated\n",
            audits.len() - shown
        ));
    }
    out.trim_end().to_string()
}

/// The argv for one probe: `(program, args)`, or `None` while the lane has no
/// binary (or, for semgrep, no `config` — it never falls back to `auto`).
/// Pure by construction — the tool's only spawn policy, so it is asserted
/// without spawning anything. `config` must already have passed
/// [`check_semgrep_config`]; `pattern` is ignored by the semgrep lane.
///
/// - ast-grep: `run --pattern <pattern> --json [--lang <lang>] <path>`
/// - semgrep: `--json --quiet --metrics=off --config <config> <path>`
pub fn command_for(
    lane: Lane,
    bins: &Bins,
    pattern: &str,
    lang: Option<&str>,
    config: Option<&str>,
    path: &str,
) -> Option<(PathBuf, Vec<String>)> {
    let program = bins.binary(lane)?.to_path_buf();
    let args = match lane {
        Lane::AstGrep => {
            let mut a: Vec<String> = vec![
                "run".into(),
                "--pattern".into(),
                pattern.into(),
                "--json".into(),
            ];
            if let Some(l) = lang {
                a.push("--lang".into());
                a.push(l.into());
            }
            a.push(path.into());
            a
        }
        Lane::Semgrep => vec![
            "--json".into(),
            "--quiet".into(),
            "--metrics=off".into(),
            "--config".into(),
            config?.into(),
            path.into(),
        ],
    };
    Some((program, args))
}

/// A semgrep `--config` is accepted only when it names a local rules file or
/// directory under `cwd`. `auto`, registry ids and URLs make semgrep fetch
/// rules over the network, which a Read-classified probe must never do.
pub fn check_semgrep_config(config: &str, cwd: &Path) -> Result<(), String> {
    let c = config.trim();
    let lower = c.to_ascii_lowercase();
    let remote = lower == "auto"
        || lower.contains("://")
        || ["p/", "r/", "s/"].iter().any(|p| lower.starts_with(p));
    if remote {
        return Err(format!(
            "struct_search: semgrep `config` `{c}` would fetch rules over the network — \
             pass a local rules file or directory (e.g. `.semgrep.yml`)."
        ));
    }
    if !cwd.join(c).exists() {
        return Err(format!(
            "struct_search: semgrep `config` `{c}` is not a local rules file or directory \
             under the working directory."
        ));
    }
    Ok(())
}

/// Tool entry: detect the operator's backends, then run the lane.
pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    run_with(input, ctx, &Bins::detect())
}

/// Backend-injecting entry (tests, future config plumbing). Argument errors
/// are tool errors; every backend problem is a fail-open note.
pub fn run_with(input: &Value, ctx: &mut ToolCtx, bins: &Bins) -> ToolOutput {
    let lane = match input.get("lane").and_then(Value::as_str) {
        None => Lane::AstGrep,
        Some(s) => match Lane::parse(s) {
            Ok(l) => l,
            // A mistyped lane is the caller's error — say so, list the lanes.
            Err(e) => return ToolOutput::err(format!("struct_search: {e}")),
        },
    };
    let nonblank = |key: &str| {
        input
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    };
    let lang = nonblank("lang");
    let config = nonblank("config");
    let path = nonblank("path").unwrap_or(".");
    let limit = opt_u64(input, "limit");

    let pattern = match lane {
        Lane::AstGrep => {
            let pattern = match need_str(input, "pattern") {
                Ok(p) => p.trim(),
                Err(_) => {
                    return ToolOutput::err(
                        "struct_search: the ast-grep lane needs `pattern` — real code to \
                         match, e.g. `foo($A)`."
                            .to_string(),
                    )
                }
            };
            if pattern.is_empty() {
                return ToolOutput::err(
                    "struct_search: `pattern` is empty — pass real code to match, e.g. `foo($A)` \
                     (`$A` is a metavariable, not a regex group)."
                        .to_string(),
                );
            }
            pattern
        }
        Lane::Semgrep => {
            let Some(c) = config else {
                return ToolOutput::err(
                    "struct_search: the semgrep lane needs `config` — a local rules file or \
                     directory (e.g. `.semgrep.yml`)."
                        .to_string(),
                );
            };
            if let Err(e) = check_semgrep_config(c, &ctx.cwd) {
                return ToolOutput::err(e);
            }
            ""
        }
    };

    if bins.binary(lane).is_none() {
        return ToolOutput::ok(format!(
            "struct_search: the {} lane has no binary — none run (fail-open). Set {} to its path, \
             or leave it unset and keep {} on PATH.",
            lane.as_str(),
            lane.env(),
            lane.path_names().join(" / ")
        ));
    }
    let Some((program, args)) = command_for(lane, bins, pattern, lang, config, path) else {
        return ToolOutput::err(format!(
            "struct_search: no {} command could be built.",
            lane.as_str()
        ));
    };

    probe(lane, &program, &args, ctx, pattern, path, config, limit)
}

/// Spawn one probe, cap its wall clock, and turn whatever it produced into a
/// result. Every backend failure mode is a note, never a tool error and never
/// a panic — a probe must not be the reason a turn dies. Both the wait and the
/// pipe collection are bounded, so a process the probe leaves behind cannot
/// wedge a turn either.
#[allow(clippy::too_many_arguments)]
fn probe(
    lane: Lane,
    program: &Path,
    args: &[String],
    ctx: &ToolCtx,
    pattern: &str,
    path: &str,
    config: Option<&str>,
    limit: Option<u64>,
) -> ToolOutput {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(&ctx.cwd)
        .env_clear()
        .envs(super::filter_env(std::env::vars_os(), |k| {
            ENV_ALLOW.contains(&k)
        }))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = match super::spawn_retrying_busy(&mut cmd) {
        Ok(c) => c,
        Err(e) => return ToolOutput::ok(note(lane, "the binary would not start", &e.to_string())),
    };

    // Drain both pipes on threads (a full pipe would wedge the child).
    let (out_pipe, err_pipe) = match (child.stdout.take(), child.stderr.take()) {
        (Some(o), Some(e)) => (o, e),
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            return ToolOutput::ok(note(lane, "the probe produced no readable pipe", ""));
        }
    };
    let out_rx = reader(out_pipe);
    let err_rx = reader(err_pipe);

    let deadline = Instant::now() + Duration::from_secs(PROBE_TIMEOUT_S);
    let mut timed_out = false;
    let mut wait_err: Option<String> = None;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Ok(None) => {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            Err(e) => {
                wait_err = Some(e.to_string());
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
        }
    };
    if let Some(e) = wait_err {
        return ToolOutput::ok(note(lane, "the probe could not be waited on", &e));
    }
    if timed_out {
        return ToolOutput::ok(note(
            lane,
            "the probe timed out",
            &format!(
                "killed after {}s on `{path}` — narrow `path` or `pattern`",
                PROBE_TIMEOUT_S
            ),
        ));
    }
    // The child is gone, so its pipes are at EOF — unless a grandchild still
    // holds them. Collection is bounded by a grace period rather than joined
    // forever: a probe must not wedge the turn on a pipe nobody closed.
    let Some(stdout) = out_rx.recv_timeout(PIPE_GRACE).ok() else {
        return ToolOutput::ok(note(
            lane,
            "the probe's stdout was never closed",
            "a process survived the probe and still holds the pipe; the buffer \
             was dropped to keep the turn moving",
        ));
    };
    // stderr is advisory (an excerpt in a note), so a late buffer costs the
    // excerpt, not the result.
    let stderr = err_rx.recv_timeout(PIPE_GRACE).unwrap_or_default();
    if stdout.len() > MAX_STDOUT {
        return ToolOutput::ok(note(
            lane,
            "the probe produced more stdout than one result may hold",
            &format!(
                "{} bytes over the {} byte cap — narrow `path` or `limit`",
                stdout.len(),
                MAX_STDOUT
            ),
        ));
    }

    let code = status.as_ref().and_then(|s| s.code());
    let scope = match lane {
        Lane::AstGrep => format!("for pattern `{pattern}`"),
        Lane::Semgrep => format!("for config `{}`", config.unwrap_or_default()),
    };
    let label = match lane {
        Lane::AstGrep => "matches",
        Lane::Semgrep => "findings",
    };

    if stdout.trim().is_empty() {
        // Nothing printed: only a zero exit means "clean run, no rows". A
        // non-zero exit or a signal means the scan never happened, and
        // reporting that as "no findings" would read as a clean audit.
        return ToolOutput::ok(match code {
            Some(0) => format!(
                "struct_search[{}]: no {label} {scope} in `{path}`.",
                lane.as_str()
            ),
            Some(c) => with_stderr(
                note(
                    lane,
                    &format!("the probe exited {c} without printing any result"),
                    "nothing was searched — this is not a clean result",
                ),
                &stderr,
            ),
            None => with_stderr(
                note(
                    lane,
                    "the probe was terminated by a signal before printing any result",
                    "nothing was searched",
                ),
                &stderr,
            ),
        });
    }

    let total;
    let body;
    let cap_note;
    match lane {
        Lane::AstGrep => match parse_astgrep_json(&stdout) {
            Ok(hits) => {
                let (kept, note) = cap_rows(&hits, cap_of(limit), "matches");
                total = hits.len();
                body = render_hits(&kept);
                cap_note = note;
            }
            Err(e) => {
                return ToolOutput::ok(with_stderr(
                    note(lane, "the output was unusable", &e),
                    &stderr,
                ))
            }
        },
        Lane::Semgrep => match parse_semgrep_json(&stdout) {
            Ok(audits) => {
                let (kept, note) = cap_rows(&audits, cap_of(limit), "findings");
                total = audits.len();
                body = render_audits(&kept);
                cap_note = note;
            }
            Err(e) => {
                return ToolOutput::ok(with_stderr(
                    note(lane, "the output was unusable", &e),
                    &stderr,
                ))
            }
        },
    }

    let mut text = if total == 0 {
        format!(
            "struct_search[{}]: no {label} {scope} in `{path}`.",
            lane.as_str()
        )
    } else {
        format!(
            "struct_search[{}]: {} {label} {scope} in `{path}`.\n{body}",
            lane.as_str(),
            total
        )
    };
    if let Some(n) = cap_note {
        text.push('\n');
        text.push_str(&n);
    }
    if let Some(c) = code {
        if c != 0 {
            // Expected when rows were found (semgrep), so it is evidence only.
            text.push_str(&format!(
                "\n[exit {c} — treated as evidence, not a failure: {} rows parsed]",
                total
            ));
        }
    }
    if total == 0 && !stderr.trim().is_empty() {
        text = with_stderr(text, &stderr);
    }
    ToolOutput::ok(text)
}

/// Rows the caller asked for, clamped to [`MAX_ROWS`]: `limit` can only
/// narrow, never widen, and never to zero (a probe that returns nothing is
/// not worth a spawn).
fn cap_of(limit: Option<u64>) -> usize {
    match limit {
        Some(n) => (n as usize).clamp(1, MAX_ROWS),
        None => MAX_ROWS,
    }
}

/// Keep at most `cap` rows, saying how many were dropped (never silent).
/// Private so the cap discipline is asserted without a backend.
fn cap_rows<T: Clone>(rows: &[T], cap: usize, kind: &str) -> (Vec<T>, Option<String>) {
    if rows.len() <= cap {
        return (rows.to_vec(), None);
    }
    let dropped = rows.len() - cap;
    (
        rows[..cap].to_vec(),
        Some(format!("… {dropped} more {kind} truncated")),
    )
}

/// The one shape every backend failure reports through: which lane, what
/// happened, the variable that configures that lane's binary, and the repair.
fn note(lane: Lane, what: &str, detail: &str) -> String {
    let mut s = format!("struct_search: {} lane: {what}", lane.as_str());
    if !detail.is_empty() {
        s.push_str(&format!(" ({detail})"));
    }
    s.push_str(&format!(
        " — no rows returned (fail-open). Set {} to the binary path, or leave it unset and keep \
         the binary on PATH.",
        lane.env()
    ));
    s
}

/// Append a bounded stderr excerpt — the backend usually says why it failed.
fn with_stderr(text: String, stderr: &str) -> String {
    let trimmed = stderr.trim();
    if trimmed.is_empty() {
        return text;
    }
    format!("{text}\n[stderr] {}", head_chars(trimmed, STDERR_EXCERPT))
}

/// The env-var half of binary discovery, split out so the stale-variable rule
/// is provable without mutating the process environment.
fn from_env_value(raw: Option<&str>) -> Option<PathBuf> {
    let path = PathBuf::from(raw?.trim());
    // A variable naming a file that is not there is *unconfigured*.
    if !path.is_file() {
        return None;
    }
    Some(path)
}

/// First of `names` that exists as a file on `PATH`, in the given order.
fn find_on_path(names: &[&str]) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for name in names {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

/// Read a pipe to the end. Bounded by what the backend writes; the caller
/// rejects an oversized result before parsing it.
fn read_pipe(mut pipe: impl Read) -> String {
    let mut buf = Vec::new();
    let _ = pipe.read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Start a detached reader thread for `pipe` and hand back the channel its
/// buffer arrives on. Detached on purpose: the caller bounds collection with
/// its own grace period, so a pipe nobody closes cannot wedge the turn.
fn reader(pipe: impl Read + Send + 'static) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    let _ = std::thread::spawn(move || {
        let _ = tx.send(read_pipe(pipe));
    });
    rx
}

/// First line of a (possibly multi-line) snippet, trimmed, plus ` …` when
/// more lines followed — the one-row-per-result contract, stated not hidden.
fn first_line(text: &str) -> String {
    let mut lines = text.lines();
    let head = lines.next().unwrap_or_default().trim();
    if lines.any(|l| !l.trim().is_empty()) {
        format!("{head} …")
    } else {
        head.to_string()
    }
}

/// First `n` characters of `s` (char-based — a multi-byte char is never cut).
fn head_chars(s: &str, n: usize) -> String {
    let mut out: String = s.chars().take(n).collect();
    if s.chars().nth(n).is_some() {
        out.push('…');
    }
    out
}

/// Error for a match/finding that lacks a field the shape requires: it is not
/// this backend's output, and a fabricated location would be worse.
fn missing_field(lane: Lane, idx: usize, field: &str) -> String {
    format!(
        "{}: entry #{idx} has no `{field}` — that is not {}",
        lane.as_str(),
        lane.output_hint()
    )
}

/// JSON kind of a value, for errors that say what actually arrived.
fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "struct_search".into(),
        description: concat!(
            "Search code *structurally*: `pattern` is real code with `$A` metavariables ",
            "(e.g. `foo($A, $B)`), not a regex — matches ignore formatting and comments. ",
            "The `semgrep` lane instead runs a rules-based audit scan (security and ",
            "anti-pattern findings, each with severity and check id). Backends are opt-in: ",
            "ast-grep via OVERSEER_AST_GREP (`ast-grep`/`sg` on PATH), semgrep via ",
            "OVERSEER_SEMGREP. Fails open with a note when the backend is missing."
        )
        .into(),
        input_schema: schema(
            json!({
                "pattern": {
                    "type": "string",
                    "description": "Structural pattern — real code with `$A` metavariables, e.g. `foo($A)`. Required by the ast-grep lane; the semgrep lane ignores it."
                },
                "lang": {
                    "type": "string",
                    "description": "Language the pattern is written in (ast-grep `--lang`), e.g. rust, python, ts. Inferred from the file extension when omitted."
                },
                "path": {
                    "type": "string",
                    "description": "File or directory to search, relative to the working directory (default `.`)."
                },
                "lane": {
                    "type": "string",
                    "enum": ["ast-grep", "semgrep"],
                    "description": "Which backend answers: `ast-grep` structural search (default) or `semgrep` audit scan."
                },
                "config": {
                    "type": "string",
                    "description": "Semgrep lane only (required there): a local rules file or directory (`--config`). Registry ids and URLs are refused."
                },
                "limit": {
                    "type": "integer",
                    "description": "Maximum rows to return; the cap is 50 and `limit` can only narrow it."
                }
            }),
            &[],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: None,
            sandbox: false,
            broker: None,
        }
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!(
            "p8-struct_search-{tag}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Two matches in the order ast-grep emits them (input order, not file
    /// order) so the sort is observable.
    const ASTGREP: &str = r#"[
      {"text":"foo(1, 2)","file":"src/b.rs","range":{"start":{"line":9,"column":4},"end":{"line":9,"column":13}},"language":"Rust"},
      {"text":"foo(x, y)","file":"src/a.rs","range":{"start":{"line":0,"column":0},"end":{"line":0,"column":9}},"language":"Rust"}
    ]"#;

    const SEMGREP: &str = r#"{
      "results": [
        {"check_id":"python.lang.security.audit.exec-use","path":"b.py","start":{"line":12,"col":3},
         "extra":{"message":"use of exec\nSee the docs.","severity":"WARNING"}},
        {"check_id":"generic.secrets.detected-aws-key","path":"a.py","start":{"line":4,"col":1},
         "extra":{"message":"AWS key detected","severity":"error"}}
      ],
      "errors": []
    }"#;

    fn hit(file: &str, line: u64, column: u64) -> Hit {
        Hit {
            file: file.into(),
            line,
            column,
            text: "x".into(),
        }
    }

    #[test]
    fn astgrep_fixture_parses_matches_into_one_based_sorted_locations() {
        let hits = parse_astgrep_json(ASTGREP).expect("fixture parses");
        assert_eq!(hits.len(), 2, "both matches land");
        // Sorted by (file, line, column): a.rs before b.rs, whatever the
        // backend's emission order was.
        assert_eq!(hits[0].file, "src/a.rs");
        assert_eq!((hits[0].line, hits[0].column), (1, 1), "0-based → 1-based");
        assert_eq!(hits[0].text, "foo(x, y)");
        assert_eq!(hits[1].file, "src/b.rs");
        assert_eq!((hits[1].line, hits[1].column), (10, 5));
        assert_eq!(hits[1].text, "foo(1, 2)");
    }

    #[test]
    fn astgrep_parse_rejects_malformed_json_naming_the_lane() {
        let err = parse_astgrep_json("{not json").unwrap_err();
        assert!(err.contains("ast-grep"), "{err}");
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    fn astgrep_parse_rejects_non_array_input_naming_the_lane() {
        let err = parse_astgrep_json(r#"{"results":[]}"#).unwrap_err();
        assert!(err.contains("ast-grep"), "{err}");
        assert!(err.contains("array"), "{err}");
        assert!(err.contains("an object"), "says what arrived: {err}");
    }

    #[test]
    fn astgrep_parse_names_the_missing_field_of_a_shapeless_match() {
        let err = parse_astgrep_json(r#"[{"text":"foo()"}]"#).unwrap_err();
        assert!(err.contains("`file`"), "{err}");
        let err = parse_astgrep_json(r#"[{"file":"a.rs"}]"#).unwrap_err();
        assert!(err.contains("range.start"), "{err}");
        let err =
            parse_astgrep_json(r#"[{"file":"a.rs","range":{"start":{"line":0}}}]"#).unwrap_err();
        assert!(err.contains("range.start.column"), "{err}");
    }

    #[test]
    fn astgrep_multiline_match_is_flattened_to_one_row() {
        let hits = parse_astgrep_json(
            r#"[{"file":"a.rs","text":"fn f() {\n    body();\n}","range":{"start":{"line":2,"column":0}}}]"#,
        )
        .unwrap();
        assert_eq!(hits[0].text, "fn f() { …", "one line, truncation marked");
        assert!(!render_hits(&hits).contains('\n') || render_hits(&hits).lines().count() == 1);
    }

    #[test]
    fn semgrep_fixture_parses_findings_with_severity_and_check_id() {
        let audits = parse_semgrep_json(SEMGREP).expect("fixture parses");
        assert_eq!(audits.len(), 2);
        assert_eq!(audits[0].file, "a.py");
        assert_eq!(audits[0].line, 4, "semgrep lines are already 1-based");
        assert_eq!(audits[0].severity, "ERROR", "severity normalized");
        assert_eq!(audits[0].check, "generic.secrets.detected-aws-key");
        assert_eq!(audits[0].message, "AWS key detected");
        assert_eq!(audits[1].check, "python.lang.security.audit.exec-use");
        assert_eq!(audits[1].severity, "WARNING");
        assert_eq!(audits[1].message, "use of exec …", "first line only");
        // Missing severity is stated, never quietly downgraded to INFO.
        let unknown = parse_semgrep_json(
            r#"{"results":[{"check_id":"c","path":"z.py","start":{"line":1},"extra":{}}]}"#,
        )
        .unwrap();
        assert_eq!(unknown[0].severity, "UNKNOWN");
    }

    #[test]
    fn semgrep_errors_without_results_is_an_empty_audit_list_not_a_failure() {
        // The shape semgrep emits when every rule failed to load: no
        // `results` key at all. Nothing was reported — not an error.
        let audits = parse_semgrep_json(r#"{"errors":[{"message":"invalid config"}],"paths":{}}"#)
            .expect("an errors-only report is an empty audit list");
        assert!(audits.is_empty());
        let audits = parse_semgrep_json(r#"{"results":[],"errors":[]}"#).unwrap();
        assert!(audits.is_empty());
        let audits = parse_semgrep_json(r#"{"results":null}"#).unwrap();
        assert!(audits.is_empty());
    }

    #[test]
    fn semgrep_parse_rejects_malformed_json_naming_the_lane() {
        let err = parse_semgrep_json("not json at all").unwrap_err();
        assert!(err.contains("semgrep"), "{err}");
    }

    #[test]
    fn semgrep_results_must_be_an_array_and_fields_must_be_present() {
        let err = parse_semgrep_json(r#"{"results":{"a":1}}"#).unwrap_err();
        assert!(err.contains("`results` must be an array"), "{err}");
        let err =
            parse_semgrep_json(r#"{"results":[{"check_id":"c","start":{"line":1}}]}"#).unwrap_err();
        assert!(err.contains("`path`"), "{err}");
        let err = parse_semgrep_json(r#"{"results":[{"check_id":"c","path":"p"}]}"#).unwrap_err();
        assert!(err.contains("start.line"), "{err}");
    }

    #[test]
    fn hits_and_findings_order_deterministically_by_their_own_keys() {
        // Same input twice → byte-identical output, and the sort key is
        // (file, line, column) for hits, (file, line, check) for findings.
        let hits = parse_astgrep_json(ASTGREP).unwrap();
        assert_eq!(
            render_hits(&hits),
            render_hits(&parse_astgrep_json(ASTGREP).unwrap())
        );

        let mixed = r#"[
          {"file":"z.rs","text":"c","range":{"start":{"line":1,"column":9}}},
          {"file":"z.rs","text":"a","range":{"start":{"line":1,"column":2}}},
          {"file":"z.rs","text":"b","range":{"start":{"line":0,"column":7}}}
        ]"#;
        let ordered = parse_astgrep_json(mixed).unwrap();
        assert_eq!(
            ordered.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            vec!["b", "a", "c"],
            "line order first, then column"
        );

        let audits = parse_semgrep_json(
            r#"{"results":[
                {"check_id":"zeta","path":"m.py","start":{"line":2},"extra":{"severity":"INFO"}},
                {"check_id":"alpha","path":"m.py","start":{"line":2},"extra":{"severity":"INFO"}},
                {"check_id":"beta","path":"m.py","start":{"line":1},"extra":{"severity":"ERROR"}}
            ]}"#,
        )
        .unwrap();
        assert_eq!(
            audits.iter().map(|a| a.check.as_str()).collect::<Vec<_>>(),
            vec!["beta", "alpha", "zeta"],
            "file, then line, then check id"
        );
    }

    #[test]
    fn render_hits_caps_at_max_rows_with_an_explicit_note() {
        let many: Vec<Hit> = (0..MAX_ROWS + 3)
            .map(|i| hit(&format!("f{i:02}.rs"), 1, 1))
            .collect();
        let text = render_hits(&many);
        assert_eq!(text.lines().count(), MAX_ROWS + 1, "50 rows + the note");
        assert!(
            text.ends_with("… 3 more matches truncated"),
            "the drop is stated, never silent: {text}"
        );
        let exact: Vec<Hit> = many[..MAX_ROWS].to_vec();
        assert_eq!(render_hits(&exact).lines().count(), MAX_ROWS);
        assert!(!render_hits(&exact).contains("truncated"));
    }

    #[test]
    fn render_findings_caps_at_max_rows_with_an_explicit_note() {
        let many: Vec<Audit> = (0..MAX_ROWS + 7)
            .map(|i| Audit {
                file: format!("f{i:02}.py"),
                line: 3,
                severity: "WARNING".into(),
                check: "c".into(),
                message: "m".into(),
            })
            .collect();
        let text = render_audits(&many);
        assert_eq!(text.lines().count(), MAX_ROWS + 1);
        assert!(text.ends_with("… 7 more findings truncated"), "{text}");
        assert_eq!(text.lines().next().unwrap(), "f00.py:3: WARNING c: m");
    }

    #[test]
    fn cap_rows_narrows_to_the_callers_limit_and_reports_the_drop() {
        let rows: Vec<Hit> = (0..MAX_ROWS + 3)
            .map(|i| hit(&format!("f{i:02}.rs"), 1, 1))
            .collect();
        // A caller limit below the cap narrows; above it, the cap holds.
        let (kept, note) = cap_rows(&rows, cap_of(Some(5)), "matches");
        assert_eq!(kept.len(), 5);
        assert_eq!(note.as_deref(), Some("… 48 more matches truncated"));
        assert_eq!(cap_of(Some(500)), MAX_ROWS, "limit can only narrow");
        assert_eq!(cap_of(None), MAX_ROWS);
        assert_eq!(cap_of(Some(0)), 1, "never zero rows");
        let (kept, note) = cap_rows(&rows, cap_of(None), "matches");
        assert_eq!(kept.len(), MAX_ROWS);
        assert_eq!(note.as_deref(), Some("… 3 more matches truncated"));
        let (kept, note) = cap_rows(&rows[..2], MAX_ROWS, "matches");
        assert_eq!(kept.len(), 2);
        assert_eq!(note, None, "under the cap there is nothing to report");
    }

    #[test]
    fn lane_parse_round_trips_and_rejects_unknown_names_listing_both_lanes() {
        assert_eq!(Lane::ALL.len(), 2);
        for lane in Lane::ALL {
            assert_eq!(Lane::parse(lane.as_str()).unwrap(), lane);
        }
        assert_eq!(Lane::parse("  SemGrep ").unwrap(), Lane::Semgrep);
        let err = Lane::parse("census").unwrap_err();
        assert!(err.contains("ast-grep"), "{err}");
        assert!(err.contains("semgrep"), "{err}");
        assert!(err.contains("census"), "names the offender: {err}");
        assert!(Lane::ALL[0].env().contains("AST_GREP"));
        assert!(Lane::ALL[1].env().contains("SEMGREP"));
    }

    #[test]
    fn missing_pattern_is_a_usage_error_naming_the_field() {
        let dir = tmpdir("nopattern");
        let mut c = ctx(&dir);
        let out = run_with(&json!({}), &mut c, &Bins::default());
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("pattern"), "{}", out.text);
    }

    #[test]
    fn empty_pattern_is_refused_rather_than_matching_everything() {
        let dir = tmpdir("emptypattern");
        let mut c = ctx(&dir);
        let out = run_with(&json!({"pattern": "   "}), &mut c, &Bins::default());
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("empty"), "{}", out.text);
        // An unknown lane is the caller's mistake too — and it lists them.
        let out = run_with(
            &json!({"pattern": "foo($A)", "lane": "census"}),
            &mut c,
            &Bins::default(),
        );
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text.contains("ast-grep") && out.text.contains("semgrep"),
            "{}",
            out.text
        );
    }

    #[test]
    fn unconfigured_lane_fails_open_with_a_note_naming_the_env_var() {
        let dir = tmpdir("unconfigured");
        std::fs::write(dir.join("rules.yml"), "rules: []\n").unwrap();
        let mut c = ctx(&dir);
        for lane in Lane::ALL {
            let bins = Bins::default();
            let out = run_with(
                &json!({"pattern": "foo($A)", "lane": lane.as_str(), "config": "rules.yml"}),
                &mut c,
                &bins,
            );
            assert!(!out.is_error, "a probe never dies loudly: {}", out.text);
            assert!(out.text.contains(lane.as_str()), "{}", out.text);
            assert!(out.text.contains(lane.env()), "{}", out.text);
            assert!(out.text.contains("fail-open"), "{}", out.text);
        }
    }

    #[test]
    fn bogus_binary_path_fails_open_with_a_note_naming_the_env_var() {
        let dir = tmpdir("bogus");
        let mut c = ctx(&dir);
        let bins = Bins {
            ast_grep: Some(dir.join("no-such-ast-grep")),
            semgrep: Some(dir.join("no-such-semgrep")),
        };
        std::fs::write(dir.join("rules.yml"), "rules: []\n").unwrap();
        for lane in Lane::ALL {
            let out = run_with(
                &json!({"pattern": "foo($A)", "lane": lane.as_str(), "path": "src",
                        "config": "rules.yml"}),
                &mut c,
                &bins,
            );
            assert!(
                !out.is_error,
                "a dead backend is a note, not a dead turn: {}",
                out.text
            );
            assert!(out.text.contains("fail-open"), "{}", out.text);
            assert!(
                out.text.contains(lane.env()),
                "the note names the repair: {}",
                out.text
            );
            assert!(out.text.contains("would not start"), "{}", out.text);
        }
    }

    #[test]
    fn command_for_builds_the_exact_astgrep_argv() {
        let bins = Bins {
            ast_grep: Some(PathBuf::from("/opt/sg")),
            semgrep: None,
        };
        let (program, args) =
            command_for(Lane::AstGrep, &bins, "foo($A)", None, None, "src").unwrap();
        assert_eq!(program, PathBuf::from("/opt/sg"));
        assert_eq!(args, vec!["run", "--pattern", "foo($A)", "--json", "src"]);
        let (_, args) =
            command_for(Lane::AstGrep, &bins, "foo($A)", Some("rust"), None, ".").unwrap();
        assert_eq!(
            args,
            vec![
                "run",
                "--pattern",
                "foo($A)",
                "--json",
                "--lang",
                "rust",
                "."
            ],
            "--lang sits between --json and the path"
        );
        assert!(
            command_for(Lane::Semgrep, &bins, "foo($A)", None, None, "src").is_none(),
            "an unconfigured lane has no command at all"
        );
    }

    #[test]
    fn command_for_builds_the_exact_semgrep_argv_with_metrics_off() {
        let bins = Bins {
            ast_grep: None,
            semgrep: Some(PathBuf::from("/opt/semgrep")),
        };
        let (program, args) =
            command_for(Lane::Semgrep, &bins, "", None, Some("rules.yml"), "src").unwrap();
        assert_eq!(program, PathBuf::from("/opt/semgrep"));
        assert_eq!(
            args,
            vec![
                "--json",
                "--quiet",
                "--metrics=off",
                "--config",
                "rules.yml",
                "src"
            ]
        );
        assert!(
            command_for(Lane::Semgrep, &bins, "", None, None, "src").is_none(),
            "no config, no command — semgrep never falls back to `auto`"
        );
        assert!(command_for(Lane::AstGrep, &bins, "x", None, None, ".").is_none());
    }

    /// A stand-in backend: records its argv to `argv.txt` in the cwd and
    /// prints `stdout`.
    #[cfg(unix)]
    fn fake_bin(dir: &Path, name: &str, stdout: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = dir.join(name);
        std::fs::write(
            &bin,
            format!("#!/bin/sh\necho \"$@\" > argv.txt\nprintf '%s' '{stdout}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    #[cfg(unix)]
    #[test]
    fn semgrep_refuses_network_configs_before_spawning() {
        let dir = tmpdir("semgrep-net");
        let bins = Bins {
            ast_grep: None,
            semgrep: Some(fake_bin(&dir, "semgrep", r#"{"results":[]}"#)),
        };
        let mut c = ctx(&dir);
        for config in [
            "auto",
            " AUTO ",
            "p/security-audit",
            "r/python.lang.security.audit.exec-use",
            "s/someone:ruleset",
            "https://example.com/rules.yml",
            "http://example.com/rules.yml",
        ] {
            let out = run_with(&json!({"lane": "semgrep", "config": config}), &mut c, &bins);
            assert!(out.is_error, "{config}: {}", out.text);
            assert!(out.text.contains("local"), "{config}: {}", out.text);
            assert!(!dir.join("argv.txt").exists(), "{config} spawned semgrep");
        }
        let out = run_with(&json!({"lane": "semgrep"}), &mut c, &bins);
        assert!(out.is_error, "a missing config is refused: {}", out.text);
        assert!(out.text.contains("config"), "{}", out.text);
        let out = run_with(
            &json!({"lane": "semgrep", "config": "no-such-rules.yml"}),
            &mut c,
            &bins,
        );
        assert!(out.is_error, "a config that is not on disk: {}", out.text);
        assert!(!dir.join("argv.txt").exists());
    }

    #[cfg(unix)]
    #[test]
    fn semgrep_runs_a_local_config_with_metrics_off_and_no_pattern() {
        let dir = tmpdir("semgrep-local");
        std::fs::write(dir.join("rules.yml"), "rules: []\n").unwrap();
        let bins = Bins {
            ast_grep: None,
            semgrep: Some(fake_bin(&dir, "semgrep", r#"{"results":[]}"#)),
        };
        let mut c = ctx(&dir);
        let out = run_with(
            &json!({"lane": "semgrep", "config": "rules.yml"}),
            &mut c,
            &bins,
        );
        assert!(!out.is_error, "{}", out.text);
        let argv = std::fs::read_to_string(dir.join("argv.txt")).unwrap();
        assert!(argv.contains("--metrics=off"), "{argv}");
        assert!(argv.contains("--config rules.yml"), "{argv}");
    }

    /// Child-process probe with a non-UTF-8 env value (see bash's twin).
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_env_value_does_not_kill_the_probe() {
        use std::os::unix::ffi::OsStringExt;
        const MARK: &str = "LC_OVERSEER_T12_PROBE";
        if std::env::var_os(MARK).is_some() {
            let dir = tmpdir("nonutf8");
            let bins = Bins {
                ast_grep: Some(fake_bin(&dir, "sg", "[]")),
                semgrep: None,
            };
            let out = run_with(&json!({"pattern": "foo($A)"}), &mut ctx(&dir), &bins);
            assert!(!out.is_error, "{}", out.text);
            assert!(dir.join("argv.txt").exists(), "the probe really ran");
            return;
        }
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::struct_search::tests::a_non_utf8_env_value_does_not_kill_the_probe",
                "--test-threads=1",
            ])
            .env(MARK, std::ffi::OsString::from_vec(vec![b'f', 0xff]))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
    }

    #[test]
    fn stale_env_var_pointing_at_a_missing_file_is_unconfigured() {
        let dir = tmpdir("stale");
        assert_eq!(from_env_value(Some("/no/such/ast-grep-xyz")), None);
        assert_eq!(from_env_value(Some("")), None);
        assert_eq!(from_env_value(Some("   ")), None);
        assert_eq!(from_env_value(None), None);
        // A real file resolves, and its value is trimmed.
        let real = dir.join("ast-grep-stub");
        std::fs::write(&real, "#!/bin/sh\n").unwrap();
        let raw = format!("  {}  ", real.display());
        assert_eq!(from_env_value(Some(&raw)), Some(real));
        // Discovery order is a constant, never a directory listing order.
        assert_eq!(AST_GREP_NAMES, ["ast-grep", "sg"]);
    }

    #[test]
    fn spec_declares_the_two_lanes_and_leaves_pattern_optional() {
        let s = spec();
        assert_eq!(s.name, "struct_search");
        assert!(s.description.contains("structurally"), "{}", s.description);
        assert!(s.description.contains("metavariable"), "{}", s.description);
        assert!(s.description.contains(ENV_AST_GREP), "{}", s.description);
        assert!(s.description.contains(ENV_SEMGREP), "{}", s.description);
        assert_eq!(s.input_schema["required"], json!([]));
        assert_eq!(
            s.input_schema["properties"]["lane"]["enum"],
            json!(["ast-grep", "semgrep"])
        );
        assert_eq!(s.input_schema["additionalProperties"], json!(false));
        assert!(!s.input_schema["properties"]["config"]["description"]
            .as_str()
            .unwrap()
            .contains("auto"));
    }
}
