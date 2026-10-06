//! Subagents (playbook Ch.3 §9.6, P3.4).
//!
//! Any subtask expected to read >~20–30K tokens of material runs in an
//! *isolated* context and returns ≤1–2K tokens: the parent gets a compact
//! digest plus a path to the subagent's full event log (share traces, not
//! summaries — Cognition).
//!
//! Modes on one tool:
//! - `read` (default): read/grep/glob only — writes stay single-threaded
//!   in the parent so two agents can never race on the same file.
//! - `write`: full tool registry inside an isolated `git worktree` under
//!   the session dir — the subagent's edits land on a scratch branch,
//!   never in the user's checkout; the digest carries a bounded diffstat.
//!   `verify: true` chains a verifier on that worktree.
//! - `verify`: read tools + `bash` in a fresh context; reviews a diff
//!   (a writer's, via `target`, or the working tree) and must end with a
//!   JSON verdict the engine parses (`verify`).
//! - `consult`: one heavy-tier request, no tools (`consult`).
//! - `background: true`: fire-and-notify. The call returns a task id
//!   immediately; the subagent runs on a thread and its digest lands in
//!   the parent's context at a later step boundary via `SubagentDone`.
//!   Fan-out is bounded by `AgentConfig::max_bg_subagents`.
//! - `resume: "task-N"` continues that subagent's own log.
//!
//! Every spawn gets a tier (light/standard/heavy → model + effort,
//! [`route`]) and a cap drawn from the parent's budget ([`budget`]).
//! Every dir is `subagents/task-N` with a `task.json` record
//! ([`sidecar`]); the result ends with a [`Footer`] line.
//!
//! `action: "cancel"` stops a background task at its next step boundary;
//! a parent interrupt reaches every subagent the same way (each spawn's
//! [`Control`](crate::control::Control) is a registered child).

// DEFERRED(orchestration): task DAG (after:) and fan-out with merge — gate: wave B
// DEFERRED(orchestration): shared scratchpad — gate: wave B
// DEFERRED(orchestration): writer retry in a fresh worktree — gate: wave B

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use serde_json::{json, Value};

use super::{middle_truncate, need_str, opt_u64, schema, ToolCtx, ToolOutput};
use crate::agent::{AgentConfig, RunOutcome};
use crate::profile::Tier;
use crate::provider::ToolSpec;

pub mod budget;
mod consult;
pub mod footer;
pub mod route;
pub mod sidecar;
mod verify;

pub use budget::SpendAccount;
pub use footer::Footer;
pub use route::TaskMode;
pub use sidecar::done_marker;

use budget::{default_cap, MIN_CAP_USD};
use route::Route;
use sidecar::{Sidecar, State};

/// Digest cap ≈2K tokens — the quarantine contract: the parent receives a
/// bounded result plus the trace path, never the raw transcript.
const RETURN_CAP: usize = 8_000;
/// A chained verifier's share of its writer's digest.
const CHAINED_VERIFY_CAP: usize = 2_000;
const DEFAULT_STEPS: u64 = 10;
const MAX_STEPS: u64 = 20;

/// Per-batch subagent handles the agent lends its tools: the
/// session-monotonic spawn counter, the parent's spend account and its
/// steering handle (each spawn registers a child of it by task id).
#[derive(Debug, Clone, Default)]
pub struct SubagentCtx {
    pub seq: u64,
    pub spend: Option<Arc<SpendAccount>>,
    pub control: crate::control::Control,
}

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "task".into(),
        description: concat!(
            "Subagent in a fresh context; returns a digest + trace path. ",
            "Spends your budget."
        )
        .into(),
        input_schema: schema(
            json!({
                "prompt": {
                    "type": "string",
                    "description": "Self-contained brief."
                },
                "action": {
                    "type": "string",
                    "description": "cancel: stop task `id`."
                },
                "id": { "type": "string" },
                "mode": {
                    "type": "string",
                    "enum": ["read", "write", "verify", "consult"],
                    "description": "read: read-only. write: own worktree. verify: checks a diff, gives a verdict. consult: one no-tools call."
                },
                "tier": {
                    "type": "string",
                    "enum": ["light", "standard", "heavy"]
                },
                "background": {
                    "type": "boolean",
                    "description": "Digest arrives later as a notice."
                },
                "resume": {
                    "type": "string",
                    "description": "task-N to continue."
                },
                "target": {
                    "type": "string",
                    "description": "verify: writer task-N (default: working tree)."
                },
                "verify": {
                    "type": "boolean",
                    "description": "write: then verify it."
                },
                "max_steps": {
                    "type": "integer",
                    "description": "Default 10, max 20."
                },
                "max_cost_usd": {
                    "type": "number",
                    "description": "USD cap."
                }
            }),
            &[],
        ),
    }
}

/// Shared subtask config: tight step ceiling, same model + effort as the
/// parent (one model per subagent). P6-2: the subagent keeps a memory
/// view but sensitivity-filtered — Secret entries stay out of the
/// quarantined context (the parent's `memory_filter` is the ceiling).
fn sub_cfg(ctx: &ToolCtx, input: &Value, cwd: &Path) -> crate::agent::AgentConfig {
    let parent = ctx.agent_config.clone().unwrap_or_default();
    let filter = parent.memory_filter;
    let mut cfg = parent;
    cfg.max_steps = opt_u64(input, "max_steps")
        .unwrap_or(DEFAULT_STEPS)
        .clamp(1, MAX_STEPS) as u32;
    cfg.memory_filter = filter;
    cfg.cwd = cwd.to_path_buf();
    cfg.auto_compact = false; // 20-step ceiling can't fill a window
    cfg.is_subagent = true;
    cfg.memory_recall = false;
    // Memory v3 §10: subagents never learn — the review pass belongs to
    // the parent's session (its `is_subagent` skip is the second guard).
    cfg.learn = false;
    cfg
}

/// Filtered memory dir for a subagent spawn (P6-2): materializes a
/// sibling `<name>.filtered/` dir holding the parent's INDEX reduced to
/// the filter ceiling plus the admitted topic files. Returns None when
/// the parent has no memory dir. Best-effort — a materialization
/// failure falls back to no memory view rather than failing the spawn.
pub fn filtered_memory_dir(
    parent_mem: &std::path::Path,
    filter: crate::memory::Sensitivity,
    dest: &Path,
) -> Option<PathBuf> {
    let idx = parent_mem.join(crate::memory::INDEX_NAME);
    let text = std::fs::read_to_string(&idx).ok()?;
    std::fs::create_dir_all(dest).ok()?;
    let mut kept_lines = Vec::new();
    for line in text.lines() {
        // Pointer recognition is memory's own rule: bare names AND
        // layer-qualified ones (`semantic/x.md`); anything else is a
        // non-pointer line preserved verbatim.
        let Some(name) = crate::memory::topic_name(line).map(str::to_string) else {
            kept_lines.push(line.to_string());
            continue;
        };
        // P8-B: one rule for what a quarantined view may see — current
        // (validity window + TTL), within the sensitivity ceiling, and not
        // `regulated`. Orphan pointers keep their line (stale-pointer
        // hygiene is consolidate's job) but copy nothing.
        match crate::memory::subagent_view(parent_mem, &name, filter) {
            crate::memory::View::PointerOnly => {
                kept_lines.push(line.to_string());
                continue;
            }
            crate::memory::View::Hidden => continue,
            crate::memory::View::Admitted(body) => {
                if let Some(src) = crate::memory::layer_path(parent_mem, &name) {
                    if let Ok(rel) = src.strip_prefix(parent_mem) {
                        if let Some(parent) = rel.parent() {
                            std::fs::create_dir_all(dest.join(parent)).ok()?;
                        }
                        std::fs::write(dest.join(rel), body).ok()?;
                    }
                }
                kept_lines.push(line.to_string());
            }
        }
    }
    let mut out = kept_lines.join("\n");
    out.push('\n');
    std::fs::write(dest.join(crate::memory::INDEX_NAME), out).ok()?;
    Some(dest.to_path_buf())
}

/// A writer's scratch branch: `overseer/<session-id first 8>/task-N` —
/// branches are repo-global, so the session id keeps sessions apart.
fn writer_branch(session_dir: &Path, seq: u64) -> String {
    let sid: String = session_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(8)
        .collect();
    let sid = if sid.is_empty() {
        "session".into()
    } else {
        sid
    };
    format!("overseer/{sid}/task-{seq}")
}

/// `git worktree add` for a writer subagent on `branch`. Returns the
/// worktree path. Fails cleanly when cwd isn't a repo.
fn add_worktree(cwd: &Path, dir: &Path, branch: &str) -> Result<std::path::PathBuf, String> {
    let wt = dir.join("wt");
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["worktree", "add", "-b", branch])
        .arg(&wt)
        .output()
        .map_err(|e| format!("git worktree: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(wt)
}

fn git_ok(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// A writer that changed nothing: drop its worktree and its branch.
fn remove_writer(repo: &Path, wt: &Path, branch: &str) -> Result<(), String> {
    let wt_s = wt.display().to_string();
    git_ok(repo, &["worktree", "remove", "--force", &wt_s])?;
    git_ok(repo, &["branch", "-D", branch]).map(|_| ())
}

/// The exact commands to take or discard a writer's work.
fn merge_note(id: &str, branch: &str, wt: &Path) -> String {
    let wt_s = wt.display();
    let dirty = git_ok(wt, &["status", "--porcelain"]).map_or(true, |s| !s.trim().is_empty());
    let commit = if dirty {
        format!("commit its uncommitted edits first: `git -C {wt_s} add -A && git -C {wt_s} commit -m '{id}'`; then ")
    } else {
        String::new()
    };
    format!(
        "[to keep: {commit}`git merge {branch}` · to discard: `git worktree remove --force {wt_s} && git branch -D {branch}`]"
    )
}

/// Bounded diffstat of what a writer subagent changed in its worktree.
fn worktree_diffstat(wt: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(wt)
        .args(["diff", "--stat", "HEAD"])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let s = String::from_utf8_lossy(&o.stdout);
            middle_truncate(&s, 4_000)
        }
        _ => "(diffstat unavailable)".into(),
    }
}

/// The branch + diffstat block a writer subagent's result carries, so its
/// worktree changes are visible to the parent (foreground and background).
fn worktree_note(branch: &str, wt: &Path) -> String {
    format!(
        "\n\n[worktree branch `{branch}` at {} — changes:\n{}]",
        wt.display(),
        worktree_diffstat(wt)
    )
}

/// One attempt's result (an agent loop or a consult call).
pub(super) struct Attempt {
    text: String,
    outcome: Result<RunOutcome, Failure>,
    /// This attempt's spend (its dir's ledger growth).
    cost: f64,
    verdict: Option<verify::Verdict>,
    notes: Vec<String>,
}

/// An attempt that never produced a run outcome.
#[derive(Debug)]
pub(super) enum Failure {
    Error(String),
    /// Refused before any call because it could exceed its cap: a bigger
    /// tier would only cost more, so this never escalates.
    CapRefused(String),
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Failure::Error(m) | Failure::CapRefused(m) => f.write_str(m),
        }
    }
}

impl Attempt {
    fn failed(e: String) -> Self {
        Self::ending(Failure::Error(e))
    }

    fn refused(e: String) -> Self {
        Self::ending(Failure::CapRefused(e))
    }

    fn ending(f: Failure) -> Self {
        Attempt {
            text: String::new(),
            outcome: Err(f),
            cost: 0.0,
            verdict: None,
            notes: Vec::new(),
        }
    }
}

fn status(o: &Result<RunOutcome, Failure>) -> &'static str {
    match o {
        Ok(RunOutcome::Completed { .. }) => "completed",
        Ok(RunOutcome::StepBudgetExceeded { .. }) => "max_steps",
        Ok(RunOutcome::CostBudgetExceeded { .. }) => "max_cost",
        Ok(RunOutcome::Stuck { .. }) => "stuck",
        Ok(RunOutcome::EmptyResponse { .. }) => "empty",
        Ok(RunOutcome::VerifyFailed { .. }) => "verify_failed",
        Ok(RunOutcome::Interrupted { .. }) => "interrupted",
        Ok(RunOutcome::Provider(_)) => "provider_error",
        Err(Failure::CapRefused(_)) => "refused",
        Err(Failure::Error(_)) => "error",
    }
}

/// Endings a stronger model might get past.
fn escalation_reason(o: &Result<RunOutcome, Failure>) -> Option<&'static str> {
    match o {
        Ok(RunOutcome::StepBudgetExceeded { .. }) => Some("step cap"),
        Ok(RunOutcome::Stuck { .. }) => Some("stuck"),
        Ok(RunOutcome::EmptyResponse { .. }) => Some("empty response"),
        Ok(RunOutcome::Provider(_)) | Err(Failure::Error(_)) => Some("run error"),
        _ => None,
    }
}

/// Everything a spawn needs, owned so a background thread can take it.
struct Env {
    provider: Arc<dyn crate::provider::Provider>,
    account: Arc<SpendAccount>,
    /// The spawning agent's config: tier resolution reads it.
    parent: AgentConfig,
    /// `sub_cfg` output; each attempt sets cwd/model/effort/cap on a copy.
    base_cfg: AgentConfig,
    subagents_dir: PathBuf,
    parent_cwd: PathBuf,
    /// The parent's steering handle (this spawn is registered under it).
    parent_control: crate::control::Control,
    /// This spawn's own handle: `action=cancel` or a parent interrupt
    /// sets it, and every attempt of the job stops at its next boundary.
    cancel: crate::control::Control,
}

struct Job {
    id: String,
    mode: TaskMode,
    route: Route,
    cap: f64,
    prompt: String,
    cwd: PathBuf,
    base: Option<String>,
    branch: Option<String>,
    /// The dir a resume continues (the task's, or its escalation attempt's).
    resume: Option<String>,
    /// Pre-claimed id of the chained verifier (`verify: true`).
    verify_id: Option<String>,
}

fn policy_for(cfg: &AgentConfig) -> crate::perm::Policy {
    if cfg.full_access {
        return crate::perm::Policy::allow_all();
    }
    let mut pol = crate::perm::Policy::preset(cfg.policy_preset, cfg.cwd.clone());
    // Every mode (verify and resumes too) sees memory read-only.
    pol.memory_readonly = true;
    // F1: draft gate propagates — a subagent inherits the parent's
    // persona verdict so unapproved drafts stay closed there too.
    pol.persona_dir = cfg.persona_dir.clone();
    pol.persona_approved = cfg
        .persona_dir
        .as_deref()
        .is_some_and(crate::onboard::all_approved);
    pol
}

/// Run one attempt in `dir`: a consult call, or an agent loop with the
/// mode's registry (fresh, or continuing the dir's log).
#[allow(clippy::too_many_arguments)]
fn run_once(
    env: &Env,
    mode: TaskMode,
    dir: &Path,
    route: &Route,
    cap: f64,
    prompt: &str,
    cwd: &Path,
    base: Option<&str>,
    continuing: bool,
) -> Attempt {
    if mode == TaskMode::Consult {
        return consult::call(env.provider.as_ref(), route, prompt, dir, cwd, cap);
    }
    let prior = if continuing {
        sidecar::ledger_total(dir)
    } else {
        0.0
    };
    let before = (mode == TaskMode::Verify)
        .then(|| verify::snapshot(cwd))
        .flatten();
    let first = match mode {
        TaskMode::Verify if continuing => format!("{prompt}\n\n{}", verify::CONTRACT),
        TaskMode::Verify => match verify::brief(prompt, cwd, base.unwrap_or("HEAD")) {
            Ok(b) => b,
            Err(e) => {
                return Attempt {
                    verdict: Some(verify::parse("")),
                    ..Attempt::failed(e)
                }
            }
        },
        _ => prompt.to_string(),
    };
    let mut cfg = env.base_cfg.clone();
    cfg.cwd = cwd.to_path_buf();
    cfg.model = route.model.clone();
    cfg.effort = route.effort;
    // The subagent's own ledger holds its earlier runs: cap this run.
    cfg.max_cost_usd = prior + cap;
    let mut registry = mode.registry(policy_for(&cfg), &cfg.cwd);
    // Ablation propagates: a component disabled for the parent is
    // disabled for every subagent too.
    registry.disable(&cfg.disabled_tools);
    let id = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let agent = if continuing {
        crate::agent::Agent::resume(env.provider.clone(), cfg, dir.to_path_buf())
    } else {
        crate::agent::Agent::start(env.provider.clone(), cfg, dir.to_path_buf(), id)
    };
    let mut sub = match agent {
        Ok(a) => a.with_tools(registry),
        Err(e) => return Attempt::failed(format!("cannot start subagent: {e}")),
    };
    sub.set_control(env.cancel.clone());
    let mut sink = |_: &crate::event::Event| {};
    let outcome = sub
        .run_turn(&first, &mut sink)
        .map_err(|e| Failure::Error(e.to_string()));
    let text = sub.messages().last().map(|m| m.text()).unwrap_or_default();
    drop(sub);
    let mut a = Attempt {
        cost: (sidecar::ledger_total(dir) - prior).max(0.0),
        verdict: None,
        notes: Vec::new(),
        text,
        outcome,
    };
    if mode == TaskMode::Verify {
        let mut v = verify::parse(&a.text);
        let after = verify::snapshot(cwd);
        // verify-tamper: any change to HEAD, refs, stash or the tree
        // forces `fail` — the verifier's own claim is ignored.
        let what = match (&before, &after) {
            (Some(b), Some(af)) => {
                Some(verify::changed(b, af).join("; ")).filter(|w| !w.is_empty())
            }
            (Some(_), None) => Some("repository unreadable after the run".to_string()),
            _ => None,
        };
        if let Some(what) = what {
            v.tampered(&what);
            a.notes.push(format!("tampered: {what}"));
        }
        a.verdict = Some(v);
    }
    a
}

/// Claim a fresh sidecar'd dir for an engine-started run (escalation
/// attempt or chained verifier).
fn open_dir(env: &Env, sc: &Sidecar) -> Result<PathBuf, String> {
    let dir = env.subagents_dir.join(&sc.id);
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    sc.store(&dir)
        .map_err(|e| format!("cannot write task.json: {e}"))?;
    Ok(dir)
}

/// Mark `dir` done (`cancelled` when the job was cancelled — its
/// reservation is released then). A failed store still leaves it
/// finished for this process (never live-looking); its cap is released
/// down to what it spent and the returned note carries the error into
/// the digest.
fn finish(env: &Env, dir: &Path) -> Option<String> {
    let mut sc = Sidecar::load(dir)?;
    let cancelled = env.cancel.interrupted();
    let state = if cancelled {
        State::Cancelled
    } else {
        State::Done
    };
    let stored = sc.finish(dir, state);
    if cancelled {
        env.account.release(&sc.id);
    }
    let e = stored.err()?;
    env.account.shrink(&sc.id, sc.cost_usd);
    Some(format!(
        "[{}: finished, but task.json could not be written — {e}; its cap is released]",
        sc.id
    ))
}

/// Every dir a job can hold `running`: its own, the one a resume
/// continues, its escalation attempt and its chained verifier.
fn job_dirs(env: &Env, job: &Job) -> Vec<PathBuf> {
    let mut ids = vec![job.id.clone(), format!("{}-r1", job.id)];
    ids.extend(job.resume.clone());
    ids.extend(job.verify_id.clone());
    ids.sort();
    ids.dedup();
    ids.iter().map(|id| env.subagents_dir.join(id)).collect()
}

/// A background job's thread panicked: everything it left `running`
/// goes `dead` with its ledger total — the parent's next reconcile
/// settles that and drops the reservations — and the returned died-marker
/// text still reaches the parent as a notice.
fn panicked(env: &Env, job: &Job, payload: &(dyn std::any::Any + Send)) -> String {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "non-string panic payload".into());
    let mut out = format!("[subagent {} panicked: {msg}]", job.id);
    for dir in job_dirs(env, job) {
        if let Some(mut sc) = Sidecar::load(&dir).filter(|sc| sc.state == State::Running) {
            if let Err(e) = sc.finish(&dir, State::Dead) {
                out.push_str(&format!(
                    "\n[{}: task.json could not be written — {e}]",
                    sc.id
                ));
            }
            env.account.shrink(&sc.id, sc.cost_usd);
        }
    }
    out
}

fn record(
    parent_cwd: &Path,
    id: &str,
    mode: TaskMode,
    route: &Route,
    cap: f64,
    job: &Job,
) -> Sidecar {
    Sidecar {
        id: id.to_string(),
        mode,
        tier: route.tier,
        model: route.model.clone(),
        background: false,
        worktree: (job.cwd != parent_cwd).then(|| job.cwd.clone()),
        branch: job.branch.clone(),
        base: job.base.clone(),
        cap_usd: cap,
        process_nonce: sidecar::process_nonce().to_string(),
        state: State::Running,
        cost_usd: 0.0,
        run: 1,
        escalated_to: None,
    }
}

/// Run a job to its final text (digest + notes + footer), escalating or
/// chaining a verifier as asked; every sidecar it touched ends `done`.
fn execute(env: &Env, job: &Job) -> String {
    let dir = env.subagents_dir.join(&job.id);
    let run_id = job.resume.clone().unwrap_or_else(|| job.id.clone());
    let run_dir = env.subagents_dir.join(&run_id);
    let mut route = job.route.clone();
    let mut a = run_once(
        env,
        job.mode,
        &run_dir,
        &route,
        job.cap,
        &job.prompt,
        &job.cwd,
        job.base.as_deref(),
        job.resume.is_some(),
    );
    let mut cost = a.cost;
    let mut notes: Vec<String> = route.note.iter().map(|n| format!("[tier: {n}]")).collect();
    if job.resume.is_none() && job.mode.escalates() {
        if let (Some(reason), Some(next)) = (escalation_reason(&a.outcome), route.tier.up()) {
            let rid = format!("{}-r1", job.id);
            let want = default_cap(job.mode, next);
            match env.account.grant(&rid, want, want) {
                Ok(cap) => {
                    let r2 = route::resolve(next, &env.parent);
                    match open_dir(env, &record(&env.parent_cwd, &rid, job.mode, &r2, cap, job)) {
                        Ok(rdir) => {
                            if let Some(mut sc) = Sidecar::load(&dir) {
                                sc.escalated_to = Some(rid.clone());
                                sc.tier = r2.tier;
                                sc.model = r2.model.clone();
                                let _ = sc.store(&dir);
                            }
                            a = run_once(
                                env,
                                job.mode,
                                &rdir,
                                &r2,
                                cap,
                                &job.prompt,
                                &job.cwd,
                                job.base.as_deref(),
                                false,
                            );
                            notes.extend(finish(env, &rdir));
                            cost += a.cost;
                            notes.push(format!(
                                "[escalated {}→{}: {reason}; trace: {}]",
                                route.tier.as_str(),
                                next.as_str(),
                                rdir.display()
                            ));
                            route = r2;
                        }
                        Err(e) => {
                            env.account.release(&rid);
                            notes.push(format!("[not escalated: {e}]"));
                        }
                    }
                }
                Err(left) => notes.push(format!(
                    "[not escalated ({reason}): ${left:.4} left, {} needs ${want:.2}]",
                    next.as_str()
                )),
            }
        }
    }
    if let Err(e) = &a.outcome {
        notes.push(format!("[error: {e}]"));
    } else if let Ok(RunOutcome::Provider(e)) = &a.outcome {
        notes.push(format!("[provider error: {e}]"));
    }
    notes.extend(a.notes.drain(..).map(|n| format!("[{n}]")));
    let cancelled = env.cancel.interrupted();
    let writer = job.branch.as_ref().filter(|_| job.mode == TaskMode::Write);
    let changed = (writer.is_some() || job.verify_id.is_some())
        && verify::has_changes(&job.cwd, job.base.as_deref().unwrap_or("HEAD"));
    if let Some(branch) = writer.filter(|_| changed) {
        notes.push(worktree_note(branch, &job.cwd).trim().to_string());
    }
    let mut verdict = a.verdict.as_ref().map(|v| v.verdict.clone());
    let mut tampered = a.verdict.as_ref().and_then(|v| v.tampered.clone());
    let unchanged = job.verify_id.is_some() && !changed;
    if unchanged {
        notes.push("[verify skipped: no changes]".into());
    } else if job.verify_id.is_some() && cancelled {
        notes.push("[verify skipped: cancelled]".into());
    }
    if let Some(vid) = job.verify_id.as_ref().filter(|_| !unchanged && !cancelled) {
        let vroute = route::resolve(Tier::Standard, &env.parent);
        match env.account.grant(
            vid,
            default_cap(TaskMode::Verify, Tier::Standard),
            MIN_CAP_USD,
        ) {
            Ok(vcap) => {
                let sc = record(&env.parent_cwd, vid, TaskMode::Verify, &vroute, vcap, job);
                match open_dir(env, &sc) {
                    Ok(vdir) => {
                        let va = run_once(
                            env,
                            TaskMode::Verify,
                            &vdir,
                            &vroute,
                            vcap,
                            &job.prompt,
                            &job.cwd,
                            job.base.as_deref(),
                            false,
                        );
                        let vfinish = finish(env, &vdir);
                        let v = va.verdict.clone().unwrap_or_else(|| verify::parse(""));
                        verdict = Some(v.verdict.clone());
                        tampered = v.tampered.clone();
                        let vfooter = Footer {
                            id: vid.clone(),
                            mode: "verify".into(),
                            tier: vroute.tier.as_str().into(),
                            model: vroute.model.clone(),
                            cost_usd: Some(va.cost),
                            status: status(&va.outcome).into(),
                            verdict: Some(v.verdict.clone()),
                            tampered: v.tampered.clone(),
                            trace: vdir.display().to_string(),
                        };
                        let mut block = format!(
                            "[verify {vid}] {}\n{}",
                            v.summary(),
                            middle_truncate(&va.text, CHAINED_VERIFY_CAP)
                        );
                        for n in &va.notes {
                            block.push_str(&format!("\n[{n}]"));
                        }
                        if let Some(n) = vfinish {
                            block.push_str(&format!("\n{n}"));
                        }
                        block.push_str(&format!(
                            "\n{}",
                            vfooter.render().replacen("[task-", "(task-", 1)
                        ));
                        notes.push(block);
                    }
                    Err(e) => {
                        env.account.release(vid);
                        notes.push(format!("[verify skipped: {e}]"));
                    }
                }
            }
            Err(left) => notes.push(format!("[verify skipped: ${left:.4} left]")),
        }
    }
    // worktree-branch-collision: a writer with no diff against its base
    // leaves nothing behind; one with a diff keeps both, and says how to
    // take or drop them.
    if let Some(branch) = writer {
        if changed {
            notes.push(merge_note(&job.id, branch, &job.cwd));
        } else {
            match remove_writer(&env.parent_cwd, &job.cwd, branch) {
                Ok(()) => {
                    if let Some(mut sc) = Sidecar::load(&dir) {
                        sc.worktree = None;
                        let _ = sc.store(&dir);
                    }
                    notes.push(format!(
                        "[no changes: worktree removed, branch `{branch}` deleted]"
                    ));
                }
                Err(e) => notes.push(format!("[no changes, but cleanup failed — {e}]")),
            }
        }
    }
    if run_dir != dir {
        notes.extend(finish(env, &run_dir));
    }
    notes.extend(finish(env, &dir));
    env.parent_control.forget(&job.id);

    let mut out = String::new();
    if let Some(v) = &a.verdict {
        out.push_str(&v.summary());
        out.push('\n');
    }
    out.push_str(&middle_truncate(&a.text, RETURN_CAP));
    for n in notes {
        out.push_str("\n\n");
        out.push_str(&n);
    }
    let footer = Footer {
        id: job.id.clone(),
        mode: job.mode.as_str().into(),
        tier: route.tier.as_str().into(),
        model: route.model.clone(),
        cost_usd: Some(cost),
        status: if cancelled {
            "cancelled".into()
        } else {
            status(&a.outcome).into()
        },
        verdict,
        tampered,
        trace: dir.display().to_string(),
    };
    out.push_str("\n\n");
    out.push_str(&footer.render());
    out
}

fn head_sha(cwd: &Path) -> Result<String, String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map_err(|e| format!("git: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "needs a git repo with a commit at {} — {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Where a spawn runs: cwd plus the worktree record it carries.
struct Placement {
    cwd: PathBuf,
    worktree: Option<PathBuf>,
    branch: Option<String>,
    base: Option<String>,
}

fn load_task(subagents_dir: &Path, id: &str, what: &str) -> Result<Sidecar, String> {
    if sidecar::task_seq(id).is_none() {
        return Err(format!("{what}: `{id}` is not a task id (task-N)"));
    }
    Sidecar::load(&subagents_dir.join(id)).ok_or_else(|| format!("{what}: no task {id}"))
}

fn existing_worktree(sc: &Sidecar) -> Result<Placement, String> {
    match &sc.worktree {
        Some(wt) if wt.is_dir() => Ok(Placement {
            cwd: wt.clone(),
            worktree: Some(wt.clone()),
            branch: sc.branch.clone(),
            base: sc.base.clone(),
        }),
        _ => Err("worktree gone — start a new task".into()),
    }
}

fn place(
    mode: TaskMode,
    prior: Option<&Sidecar>,
    target: Option<&str>,
    seq: u64,
    ctx: &ToolCtx,
    subagents_dir: &Path,
) -> Result<Placement, String> {
    let here = || Placement {
        cwd: ctx.cwd.clone(),
        worktree: None,
        branch: None,
        base: None,
    };
    match (mode, prior) {
        (TaskMode::Write, None) => {
            let base = head_sha(&ctx.cwd).map_err(|e| format!("mode=write {e}"))?;
            let branch = writer_branch(&ctx.session_dir, seq);
            let wt = add_worktree(&ctx.cwd, &subagents_dir.join(format!("wt-{seq}")), &branch)
                .map_err(|e| format!("mode=write needs a git worktree — {e}"))?;
            Ok(Placement {
                cwd: wt.clone(),
                worktree: Some(wt),
                branch: Some(branch),
                base: Some(base),
            })
        }
        (TaskMode::Write, Some(p)) => existing_worktree(p),
        (TaskMode::Verify, None) => match target {
            Some(t) => {
                let sc = load_task(subagents_dir, t, "target")?;
                if sc.mode != TaskMode::Write {
                    return Err(format!(
                        "target: {t} is a {} task, not write",
                        sc.mode.as_str()
                    ));
                }
                if sc.is_live() {
                    return Err(format!("target: {t} is still running"));
                }
                existing_worktree(&sc)
            }
            None => {
                head_sha(&ctx.cwd).map_err(|e| format!("mode=verify {e}"))?;
                Ok(Placement {
                    base: Some("HEAD".into()),
                    ..here()
                })
            }
        },
        (TaskMode::Verify, Some(p)) if p.worktree.is_some() => existing_worktree(p),
        (TaskMode::Verify, Some(p)) => Ok(Placement {
            base: p.base.clone(),
            ..here()
        }),
        (TaskMode::Read | TaskMode::Consult, _) => Ok(here()),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    match input.get("action").and_then(Value::as_str) {
        None | Some("spawn") => {}
        Some("cancel") => return cancel(input, ctx),
        Some(other) => return ToolOutput::err(format!("task: unknown action `{other}`")),
    }
    let prompt = match need_str(input, "prompt") {
        Ok(p) => p,
        Err(e) => return e,
    };
    match spawn(prompt, input, ctx) {
        Ok(text) => ToolOutput::ok(text),
        Err(e) => ToolOutput::err(format!("task: {e}")),
    }
}

/// `action=cancel id=task-N`: interrupt a running background task. It
/// stops at its next step boundary, goes `cancelled`, releases its
/// reservation and still delivers a `SubagentDone` (status cancelled).
fn cancel(input: &Value, ctx: &ToolCtx) -> ToolOutput {
    let id = match need_str(input, "id") {
        Ok(i) => i,
        Err(e) => return e,
    };
    let sc = match load_task(&ctx.session_dir.join("subagents"), id, "cancel") {
        Ok(sc) => sc,
        Err(e) => return ToolOutput::err(format!("task: {e}")),
    };
    if !sc.is_live() {
        return ToolOutput::err(format!("task: cancel: {id} is not running"));
    }
    match ctx.subagents.control.child_of(id) {
        Some(c) => {
            c.interrupt();
            ToolOutput::ok(format!(
                "Cancelling {id}: it stops at its next step boundary; its notice \
                 still arrives (status cancelled)."
            ))
        }
        None => ToolOutput::err(format!("task: cancel: {id} has no handle in this process")),
    }
}

fn spawn(prompt: &str, input: &Value, ctx: &mut ToolCtx) -> Result<String, String> {
    let provider = ctx
        .provider
        .clone()
        .ok_or("no provider handle in this tool context")?;
    let parent = ctx
        .agent_config
        .clone()
        .ok_or("no agent config in this tool context")?;
    let str_arg = |k: &str| input.get(k).and_then(Value::as_str);
    let flag = |k: &str| input.get(k).and_then(Value::as_bool).unwrap_or(false);
    let mode_in = str_arg("mode")
        .map(|m| TaskMode::parse(m).ok_or(format!("unknown mode `{m}`")))
        .transpose()?;
    let tier_in = str_arg("tier")
        .map(|t| Tier::parse(t).ok_or(format!("unknown tier `{t}`")))
        .transpose()?;
    let background = flag("background");
    let requested_cap = input.get("max_cost_usd").and_then(Value::as_f64);
    if requested_cap.is_some_and(|c| c.is_nan() || c <= 0.0) {
        return Err("max_cost_usd must be > 0".into());
    }
    let subagents_dir = ctx.session_dir.join("subagents");
    let account = ctx
        .subagents
        .spend
        .clone()
        .ok_or("no spend account in this tool context")?;

    let prior = str_arg("resume")
        .map(|id| load_task(&subagents_dir, id, "resume"))
        .transpose()?;
    if let Some(p) = &prior {
        sidecar::reap_dead(&subagents_dir);
        if Sidecar::load(&subagents_dir.join(&p.id)).is_some_and(|sc| sc.is_live()) {
            return Err(format!("{} is still running", p.id));
        }
    }
    let mode = mode_in
        .or(prior.as_ref().map(|p| p.mode))
        .unwrap_or(TaskMode::Read);
    let tier = tier_in
        .or(prior.as_ref().map(|p| p.tier))
        .unwrap_or(mode.default_tier());
    if mode == TaskMode::Consult && prior.is_some() {
        return Err("consult is one call — start a new consult".into());
    }
    if mode == TaskMode::Consult && background {
        return Err("consult is one call — background is refused".into());
    }
    if flag("verify") && mode != TaskMode::Write {
        return Err("verify: true applies to mode=write".into());
    }
    let target = str_arg("target");
    if target.is_some() && (mode != TaskMode::Verify || prior.is_some()) {
        return Err("target applies to a new mode=verify task".into());
    }
    // An untargeted verify reviews the parent's own tree; in the
    // background the lead keeps editing it, and the tamper check would
    // blame the verifier.
    let in_parent_tree = prior
        .as_ref()
        .map_or(target.is_none(), |p| p.worktree.is_none());
    if mode == TaskMode::Verify && background && in_parent_tree {
        return Err(
            "background mode=verify needs a target — an untargeted verify reviews \
             your working tree while you keep editing it; run it with \
             background=false, or target a write task"
                .into(),
        );
    }
    // Bounded fan-out — filesystem-derived (sidecars), so it survives
    // resumes and can't drift from actual thread state.
    let limit = parent.max_bg_subagents;
    if background && sidecar::in_flight(&subagents_dir) >= limit {
        return Err(format!(
            "{limit} background subagents already in flight — \
             wait for a notice or run this one with background=false"
        ));
    }

    let route = route::resolve(tier, &parent);
    let seq = ctx.subagents.seq + 1;
    let id = prior
        .as_ref()
        .map(|p| p.id.clone())
        .unwrap_or_else(|| format!("task-{seq}"));
    let want = requested_cap.unwrap_or(default_cap(mode, tier));
    let cap = account.grant(&id, want, MIN_CAP_USD).map_err(|left| {
        format!("budget left ${left:.4} is below the ${MIN_CAP_USD:.2} minimum spawn cap")
    })?;
    let placed = place(mode, prior.as_ref(), target, seq, ctx, &subagents_dir);
    let dir = subagents_dir.join(&id);
    let placed = placed.and_then(|p| {
        std::fs::create_dir_all(&dir)
            .map(|_| p)
            .map_err(|e| format!("cannot create subagent dir — {e}"))
    });
    let placed = match placed {
        Ok(p) => p,
        Err(e) => {
            account.release(&id);
            return Err(e);
        }
    };
    if prior.is_none() {
        ctx.subagents.seq = seq;
    }
    let run = prior.as_ref().map_or(1, |p| p.run + 1);
    let sc = Sidecar {
        id: id.clone(),
        mode,
        tier,
        model: route.model.clone(),
        background,
        worktree: placed.worktree.clone(),
        branch: placed.branch.clone(),
        base: placed.base.clone(),
        cap_usd: cap,
        process_nonce: sidecar::process_nonce().to_string(),
        state: State::Running,
        cost_usd: prior.as_ref().map_or(0.0, |p| p.cost_usd),
        run,
        escalated_to: prior.as_ref().and_then(|p| p.escalated_to.clone()),
    };
    // Written before any thread starts: the in-flight count reads it, so
    // a burst of spawns can't all pass the bound.
    if let Err(e) = sc.store(&dir) {
        account.release(&id);
        return Err(format!("cannot write task.json — {e}"));
    }
    let resume = prior
        .as_ref()
        .map(|p| p.escalated_to.clone().unwrap_or(p.id.clone()));
    if let Some(r) = resume.as_ref().filter(|r| **r != id) {
        let rdir = subagents_dir.join(r);
        if let Some(mut rsc) = Sidecar::load(&rdir) {
            rsc.state = State::Running;
            rsc.process_nonce = sidecar::process_nonce().to_string();
            let _ = rsc.store(&rdir);
        }
    }
    // Claim the chained verifier's id now so later spawns (and a
    // re-seeded counter) can't take it.
    let verify_id = flag("verify").then(|| {
        ctx.subagents.seq += 1;
        let vid = format!("task-{}", ctx.subagents.seq);
        let _ = std::fs::create_dir_all(subagents_dir.join(&vid));
        vid
    });

    let mut base_cfg = sub_cfg(ctx, input, &placed.cwd);
    // P6-2 filtered view: the subagent's memory dir is a reduced copy —
    // Secret entries never reach the quarantined context. A copy failure
    // falls back to no memory view (never the full parent dir).
    if let Some(parent_mem) = ctx.agent_config.as_ref().and_then(|c| c.memory_dir.clone()) {
        let dest = dir.join("memory.filtered");
        base_cfg.memory_dir = filtered_memory_dir(&parent_mem, base_cfg.memory_filter, &dest);
    } else {
        base_cfg.memory_dir = None;
    }
    base_cfg.user_memory_dir = base_cfg.user_memory_dir.take().and_then(|user| {
        filtered_memory_dir(
            &user,
            base_cfg.memory_filter,
            &dir.join("memory.user.filtered"),
        )
    });
    let env = Env {
        provider,
        account,
        parent,
        base_cfg,
        subagents_dir,
        parent_cwd: ctx.cwd.clone(),
        parent_control: ctx.subagents.control.clone(),
        cancel: ctx.subagents.control.child(&id),
    };
    let job = Job {
        id: id.clone(),
        mode,
        route: route.clone(),
        cap,
        prompt: prompt.to_string(),
        cwd: placed.cwd,
        base: placed.base,
        branch: placed.branch,
        resume,
        verify_id,
    };
    if !background {
        return Ok(execute(&env, &job));
    }
    let ack = Footer {
        id: id.clone(),
        mode: mode.as_str().into(),
        tier: tier.as_str().into(),
        model: route.model.clone(),
        cost_usd: None,
        status: "background".into(),
        verdict: None,
        tampered: None,
        trace: dir.display().to_string(),
    };
    let delivery = sidecar::Delivery::new(&dir);
    std::thread::spawn(move || {
        let _delivery = delivery;
        let text = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(&env, &job)))
            .unwrap_or_else(|p| panicked(&env, &job, p.as_ref()));
        // Marker last (after the sidecar went `done`): it is the parent
        // loop's notification. Atomic — the drain reads it as soon as it
        // exists.
        let _ = sidecar::write_marker(&dir.join(done_marker(run)), &text);
    });
    Ok(format!(
        "Background task {id} started. Its digest arrives as a notice at a \
         later step — do not block waiting for it.\n\n{}",
        ack.render()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::Usage;
    use crate::provider::{Provider, ProviderError, Request, Response, StopReason};
    use std::collections::VecDeque;
    use std::sync::Mutex;

    #[derive(Debug, Clone)]
    struct Seen {
        model: String,
        tools: Vec<String>,
        first_user: String,
        max_tokens: u32,
        effort: Option<crate::provider::Effort>,
    }

    struct Mock {
        responses: Mutex<VecDeque<Response>>,
        seen: Mutex<Vec<Seen>>,
        /// Runs at the start of every call — injects panics and faults.
        hook: Option<Box<dyn Fn() + Send + Sync>>,
    }

    impl Provider for Mock {
        fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
            if let Some(h) = &self.hook {
                h();
            }
            self.seen.lock().unwrap().push(Seen {
                model: req.model.to_string(),
                tools: req.tools.iter().map(|t| t.name.clone()).collect(),
                first_user: req.messages.first().map(|m| m.text()).unwrap_or_default(),
                max_tokens: req.max_tokens,
                effort: req.effort,
            });
            let mut q = self.responses.lock().unwrap();
            let r = if q.len() > 1 {
                q.pop_front().unwrap()
            } else {
                q.front().unwrap().clone()
            };
            Ok(r)
        }
        fn name(&self) -> &'static str {
            "mock"
        }
    }

    fn done_text(t: &str) -> Response {
        Response {
            blocks: vec![crate::ir::Block::Text { text: t.into() }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn empty() -> Response {
        Response {
            blocks: vec![],
            ..done_text("")
        }
    }

    fn call(n: usize, name: &str, input: Value, usage: Usage) -> Response {
        Response {
            blocks: vec![crate::ir::Block::ToolCall {
                id: format!("c{n}"),
                name: name.into(),
                input,
            }],
            stop_reason: StopReason::ToolUse,
            usage,
            request_bytes: 0,
            latency_ms: 0,
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-task-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Anthropic parent so the tiers resolve to distinct rows.
    fn cfg(dir: &Path) -> AgentConfig {
        AgentConfig {
            cwd: dir.to_path_buf(),
            full_access: true,
            model: "claude-fable-5".into(),
            ..Default::default()
        }
    }

    fn ctx_with(
        dir: &Path,
        responses: Vec<Response>,
        cfg: AgentConfig,
    ) -> (ToolCtx<'static>, Arc<Mock>) {
        ctx_hooked(dir, responses, cfg, None)
    }

    fn ctx_hooked(
        dir: &Path,
        responses: Vec<Response>,
        cfg: AgentConfig,
        hook: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> (ToolCtx<'static>, Arc<Mock>) {
        let mock = Arc::new(Mock {
            responses: Mutex::new(VecDeque::from(responses)),
            seen: Mutex::new(Vec::new()),
            hook,
        });
        let c = ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: Some(mock.clone()),
            subagents: SubagentCtx {
                control: Default::default(),
                seq: 0,
                spend: Some(Arc::new(SpendAccount::new(cfg.max_cost_usd, 0.0))),
            },
            agent_config: Some(cfg),
            checkpoint: None,
            sandbox: false,
            broker: None,
        };
        (c, mock)
    }

    fn ctx(dir: &Path) -> ToolCtx<'static> {
        ctx_with(dir, vec![done_text("digest body")], cfg(dir)).0
    }

    fn git(dir: &Path, args: &[&str]) {
        let st = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(["-c", "user.email=t@t", "-c", "user.name=t"])
            .args(args)
            .output()
            .unwrap()
            .status;
        assert!(st.success(), "git {args:?}");
    }

    fn repo() -> PathBuf {
        let dir = tmpdir();
        git(&dir, &["init", "-q"]);
        std::fs::write(dir.join(".gitignore"), "session/\n").unwrap();
        std::fs::write(dir.join("a.txt"), "one\n").unwrap();
        git(&dir, &["add", "."]);
        git(&dir, &["commit", "-qm", "x"]);
        dir
    }

    fn wait_for(p: &Path) {
        // Generous bound: under full-workspace parallel load a bg spawn
        // can take seconds; the loop exits the moment the file lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !p.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(p.exists(), "{} never appeared", p.display());
    }

    fn sc(dir: &Path, id: &str) -> Sidecar {
        Sidecar::load(&dir.join("session/subagents").join(id)).expect("task.json")
    }

    fn live_bg(dir: &Path, id: &str, nonce: &str) {
        let d = dir.join("session/subagents").join(id);
        std::fs::create_dir_all(&d).unwrap();
        Sidecar {
            id: id.into(),
            mode: TaskMode::Read,
            tier: Tier::Light,
            model: "m".into(),
            background: true,
            worktree: None,
            branch: None,
            base: None,
            cap_usd: 0.25,
            process_nonce: nonce.into(),
            state: State::Running,
            cost_usd: 0.0,
            run: 1,
            escalated_to: None,
        }
        .store(&d)
        .unwrap();
    }

    #[test]
    fn read_subagent_returns_bounded_digest() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "look around"}), &mut c);
        assert!(!out.is_error);
        assert!(out.text.contains("digest body"));
        let f = Footer::parse(&out.text).expect("footer");
        assert_eq!(
            (f.id.as_str(), f.mode.as_str(), f.tier.as_str()),
            ("task-1", "read", "light")
        );
        assert_eq!(f.model, "claude-haiku-4-5");
        assert_eq!(f.status, "completed");
        let s = sc(&dir, "task-1");
        assert_eq!(s.state, State::Done);
        assert_eq!(s.cap_usd, budget::LIGHT_CAP_USD);
    }

    #[test]
    fn task_spec_fits_its_budget() {
        // Same metric as the resident-budget test.
        let s = spec();
        let n = s.name.chars().count()
            + s.description.chars().count()
            + serde_json::to_string(&s.input_schema)
                .unwrap()
                .chars()
                .count();
        println!("task spec: {n} chars");
        assert!(n <= 1_000, "task spec is {n} chars");
    }

    /// Hierarchical cancel: the child blocks in its first call until its
    /// own control is interrupted, then stops at its next boundary.
    fn cancel_case(by_action: bool) {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::{Duration, Instant};
        let dir = tmpdir();
        let parent = crate::control::Control::default();
        let started = Arc::new(AtomicBool::new(false));
        let (s, h) = (started.clone(), parent.clone());
        let hook: Box<dyn Fn() + Send + Sync> = Box::new(move || {
            s.store(true, Ordering::SeqCst);
            let t = Instant::now();
            while !h.child_of("task-1").is_some_and(|c| c.interrupted())
                && t.elapsed() < Duration::from_secs(10)
            {
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        let looping = (0..40)
            .map(|n| {
                call(
                    n,
                    "glob",
                    json!({"pattern": format!("*{n}")}),
                    Usage::default(),
                )
            })
            .collect();
        let (mut c, _mock) = ctx_hooked(&dir, looping, cfg(&dir), Some(hook));
        c.subagents.control = parent.clone();
        let out = run(&json!({"prompt": "loop", "background": true}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !started.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(started.load(Ordering::SeqCst), "child never called");
        if by_action {
            let o = run(&json!({"action": "cancel", "id": "task-1"}), &mut c);
            assert!(!o.is_error, "{}", o.text);
        } else {
            parent.interrupt();
        }
        let marker = dir.join("session/subagents/task-1/done.txt");
        wait_for(&marker);
        assert_eq!(sc(&dir, "task-1").state, sidecar::State::Cancelled);
        let spend = c.subagents.spend.clone().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while spend.reserved_usd() > 0.0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(spend.reserved_usd(), 0.0);
        let done = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(Footer::parse(&done).unwrap().status, "cancelled", "{done}");
    }

    #[test]
    fn parent_interrupt_cancels_a_looping_background_child() {
        cancel_case(false);
    }

    #[test]
    fn task_action_cancel_stops_a_background_child() {
        cancel_case(true);
    }

    #[test]
    fn background_spawns_and_marks_done() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "bg work", "background": true}), &mut c);
        assert!(!out.is_error);
        assert!(out.text.contains("Background task task-1 started"));
        assert_eq!(Footer::parse(&out.text).unwrap().id, "task-1");
        let marker = dir.join("session/subagents/task-1/done.txt");
        wait_for(&marker);
        assert!(std::fs::read_to_string(&marker)
            .unwrap()
            .contains("digest body"));
        // The sidecar goes `done` before the marker: the slot is free.
        assert_eq!(sidecar::in_flight(&dir.join("session/subagents")), 0);
        assert!(sc(&dir, "task-1").background);
    }

    /// No spend account means no budget to draw from: refused, never a
    /// fresh account that treats the parent's spend as zero.
    #[test]
    fn spawn_without_a_spend_account_is_refused() {
        let dir = tmpdir();
        let (mut c, mock) = ctx_with(&dir, vec![done_text("x")], cfg(&dir));
        c.subagents.spend = None;
        let out = run(&json!({"prompt": "p"}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains("no spend account"), "{}", out.text);
        assert!(mock.seen.lock().unwrap().is_empty(), "nothing ran");
        assert!(!dir.join("session/subagents/task-1").exists());
    }

    /// A task dir that turns unwritable mid-run: the finish cannot be
    /// stored, yet the sidecar reads done (slot free), the cap is released
    /// and the digest says why.
    #[cfg(unix)]
    #[test]
    fn finish_store_failure_releases_and_never_looks_live() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmpdir();
        let task_dir = dir.join("session/subagents/task-1");
        let locked = task_dir.clone();
        let (mut c, _) = ctx_hooked(
            &dir,
            vec![done_text("digest body")],
            cfg(&dir),
            Some(Box::new(move || {
                let _ = std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o555));
            })),
        );
        let out = run(&json!({"prompt": "p"}), &mut c);
        let probe = std::fs::write(task_dir.join("probe"), "");
        std::fs::set_permissions(&task_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if probe.is_ok() {
            eprintln!("skipped: permissions are not enforced here (root?)");
            return;
        }
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("digest body"), "{}", out.text);
        assert!(
            out.text
                .contains("task-1: finished, but task.json could not be written"),
            "{}",
            out.text
        );
        let s = sc(&dir, "task-1");
        assert_eq!(s.state, State::Done);
        assert!(!s.is_live());
        let acct = c.subagents.spend.as_ref().unwrap();
        assert!(acct.reserved_usd() < 1e-9, "{}", acct.reserved_usd());
    }

    /// A panicking background thread must not strand a live-looking
    /// sidecar: it goes `dead` with its ledger total (the next drain
    /// settles it and drops the reservation) and its notice still lands.
    #[test]
    fn background_panic_marks_dead_and_leaves_a_notice() {
        let dir = tmpdir();
        let fired = std::sync::atomic::AtomicBool::new(false);
        let (mut c, _) = ctx_hooked(
            &dir,
            vec![done_text("never")],
            cfg(&dir),
            Some(Box::new(move || {
                if !fired.swap(true, std::sync::atomic::Ordering::SeqCst) {
                    panic!("boom");
                }
            })),
        );
        let out = run(&json!({"prompt": "bg", "background": true}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        let marker = dir.join("session/subagents/task-1/done.txt");
        wait_for(&marker);
        let note = std::fs::read_to_string(&marker).unwrap();
        assert_eq!(note, "[subagent task-1 panicked: boom]");
        let s = sc(&dir, "task-1");
        assert_eq!(s.state, State::Dead);
        assert!(!s.is_live());
        let subs = dir.join("session/subagents");
        assert_eq!(sidecar::in_flight(&subs), 0, "slot freed");
        let spend = c.subagents.spend.clone().unwrap();
        assert_eq!(spend.reserved_usd(), 0.0, "cap released at the panic");
        // Resume is allowed again (it gets past the liveness check).
        let out = run(&json!({"prompt": "again", "resume": "task-1"}), &mut c);
        assert!(!out.text.contains("still running"), "{}", out.text);
    }

    /// The bound counts live background tasks only; dirs a dead process
    /// left `running` are reaped rather than wedging the cap.
    #[test]
    fn fan_out_is_bounded_and_dead_tasks_free_their_slots() {
        let dir = tmpdir();
        let limit = AgentConfig::default().max_bg_subagents;
        for i in 1..limit {
            live_bg(&dir, &format!("task-{i}"), sidecar::process_nonce());
        }
        live_bg(&dir, &format!("task-{limit}"), "previous-process");
        live_bg(&dir, &format!("task-{}", limit + 1), "previous-process");
        let mut c = ctx(&dir);
        c.subagents.seq = limit as u64 + 1;
        let out = run(&json!({"prompt": "x", "background": true}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(sc(&dir, &format!("task-{limit}")).state, State::Dead);
        // All `limit` slots are now live (task-N+2 may still be running):
        // hold one more live so the next spawn must be refused.
        live_bg(&dir, "task-99", sidecar::process_nonce());
        let out = run(&json!({"prompt": "y", "background": true}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains("in flight"), "{}", out.text);
    }

    #[test]
    fn write_mode_uses_worktree() {
        let dir = repo();
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "write stuff", "mode": "write"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        // A writer that changed nothing leaves no worktree or branch.
        assert!(
            out.text
                .contains("branch `overseer/session/task-1` deleted"),
            "{}",
            out.text
        );
        assert!(!dir.join("session/subagents/wt-1/wt").exists());
        let s = sc(&dir, "task-1");
        assert_eq!(s.branch.as_deref(), Some("overseer/session/task-1"));
        assert_eq!(s.worktree, None);
        assert_eq!(s.tier, Tier::Standard);
        assert!(s.base.is_some());
    }

    /// A full-access writer's policy root is `/`, but its skills live in
    /// its worktree: the registry must look there, like the prompt does.
    #[test]
    fn write_subagent_detects_skills_in_its_worktree() {
        let dir = tmpdir();
        let skill = dir.join(".overseer/skills/demo");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: demo\ndescription: a demo skill\n---\nbody\n",
        )
        .unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["add", ".overseer"],
            vec![
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "-qm",
                "x",
            ],
        ] {
            let st = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(&args)
                .output()
                .unwrap()
                .status;
            assert!(st.success());
        }
        let (mut c, mock) = ctx_with(&dir, vec![done_text("digest body")], cfg(&dir));
        let out = run(&json!({"prompt": "write stuff", "mode": "write"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        let seen = mock.seen.lock().unwrap();
        assert!(!seen.is_empty());
        assert!(
            seen.iter().all(|s| s.tools.iter().any(|n| n == "skill")),
            "{seen:?}"
        );
    }

    #[test]
    fn background_write_mode_reports_its_worktree_in_done_txt() {
        let dir = repo();
        let (mut c, _) = ctx_with(
            &dir,
            vec![
                call(
                    1,
                    "bash",
                    json!({"command": "echo x > new.txt"}),
                    Usage::default(),
                ),
                done_text("digest body"),
            ],
            cfg(&dir),
        );
        let out = run(
            &json!({"prompt": "write stuff", "mode": "write", "background": true}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        let marker = dir.join("session/subagents/task-1/done.txt");
        wait_for(&marker);
        let done = std::fs::read_to_string(&marker).expect("done.txt");
        assert!(done.contains("digest body"), "{done}");
        assert!(
            done.contains("worktree branch `overseer/session/task-1`"),
            "{done}"
        );
        assert!(done.contains("changes:"), "{done}");
        assert!(
            done.contains("`git merge overseer/session/task-1`"),
            "{done}"
        );
        assert!(done.contains("git worktree remove --force"), "{done}");
    }

    #[test]
    fn write_mode_fails_cleanly_outside_git() {
        let dir = tmpdir(); // not a repo
        let mut c = ctx(&dir);
        let out = run(&json!({"prompt": "x", "mode": "write"}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains("mode=write"), "{}", out.text);
        // The failed spawn gave its reservation back.
        let acct = c.subagents.spend.as_ref().unwrap();
        assert_eq!(acct.reserved_usd(), 0.0);
    }

    /// A parent capped at $0.10 hands a subagent at most $0.10, whatever
    /// it asks for — and that subagent really stops there.
    #[test]
    fn subagent_cap_clamps_to_parent_remaining() {
        let dir = tmpdir();
        let parent = AgentConfig {
            max_cost_usd: 0.10,
            ..cfg(&dir)
        };
        let light = route::resolve(Tier::Light, &parent).model;
        let price = crate::profile::lookup(&light).price.input;
        let usage = Usage {
            fresh_input: (0.06 * 1_000_000.0 / price).round() as u64,
            ..Usage::default()
        };
        let (mut c, _) = ctx_with(
            &dir,
            vec![
                call(1, "glob", json!({"pattern": "*.a"}), usage),
                call(2, "glob", json!({"pattern": "*.b"}), usage),
            ],
            parent,
        );
        let out = run(&json!({"prompt": "spend", "max_cost_usd": 1.0}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(sc(&dir, "task-1").cap_usd, 0.10);
        let f = Footer::parse(&out.text).unwrap();
        assert_eq!(f.status, "max_cost", "{}", out.text);
        // task-1's cap is still reserved (no parent loop settled it): the
        // parent has nothing left to hand out.
        let out = run(&json!({"prompt": "more"}), &mut c);
        assert!(out.is_error);
        assert!(
            out.text
                .contains("budget left $0.0000 is below the $0.01 minimum"),
            "{}",
            out.text
        );
    }

    #[test]
    fn background_reservations_block_concurrent_overspend() {
        let dir = tmpdir();
        let (mut c, _) = ctx_with(
            &dir,
            vec![done_text("d")],
            AgentConfig {
                max_cost_usd: 0.30,
                ..cfg(&dir)
            },
        );
        for (i, want) in [(1, 0.25), (2, 0.05)] {
            let out = run(&json!({"prompt": "bg", "background": true}), &mut c);
            assert!(!out.is_error, "{}", out.text);
            assert!((sc(&dir, &format!("task-{i}")).cap_usd - want).abs() < 1e-9);
        }
        let out = run(&json!({"prompt": "bg", "background": true}), &mut c);
        assert!(
            out.is_error && out.text.contains("budget left"),
            "{}",
            out.text
        );
        wait_for(&dir.join("session/subagents/task-2/done.txt"));
    }

    #[test]
    fn resume_continues_the_log_with_the_modes_registry() {
        let dir = tmpdir();
        let (mut c, mock) = ctx_with(
            &dir,
            vec![done_text("first"), done_text("second")],
            cfg(&dir),
        );
        assert!(!run(&json!({"prompt": "look"}), &mut c).is_error);
        let out = run(&json!({"prompt": "look more", "resume": "task-1"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("second"));
        let events =
            crate::event::EventLog::replay(dir.join("session/subagents/task-1/events.jsonl"))
                .unwrap();
        let inputs = events
            .iter()
            .filter(|e| matches!(e.kind, crate::event::EventKind::UserInput { .. }))
            .count();
        assert_eq!(inputs, 2);
        let s = sc(&dir, "task-1");
        assert_eq!((s.run, s.state, s.tier), (2, State::Done, Tier::Light));
        let last = mock.seen.lock().unwrap().last().cloned().unwrap();
        assert!(last.tools.iter().any(|t| t == "read"), "{:?}", last.tools);
        for t in ["write", "edit", "bash", "task"] {
            assert!(!last.tools.iter().any(|x| x == t), "resumed read has {t}");
        }
        assert!(
            !dir.join("session/subagents/task-2").exists(),
            "resume keeps the id"
        );
    }

    #[test]
    fn resume_refuses_running_tasks_and_respects_the_cap() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        live_bg(&dir, "task-1", sidecar::process_nonce());
        let out = run(&json!({"prompt": "go on", "resume": "task-1"}), &mut c);
        assert!(
            out.is_error && out.text.contains("task-1 is still running"),
            "{}",
            out.text
        );
        let mut s = sc(&dir, "task-1");
        s.state = State::Done;
        s.store(&dir.join("session/subagents/task-1")).unwrap();
        for i in 2..=AgentConfig::default().max_bg_subagents + 1 {
            live_bg(&dir, &format!("task-{i}"), sidecar::process_nonce());
        }
        let out = run(
            &json!({"prompt": "go on", "resume": "task-1", "background": true}),
            &mut c,
        );
        assert!(
            out.is_error && out.text.contains("in flight"),
            "{}",
            out.text
        );
        let out = run(&json!({"prompt": "x", "resume": "../etc"}), &mut c);
        assert!(
            out.is_error && out.text.contains("not a task id"),
            "{}",
            out.text
        );
    }

    #[test]
    fn resume_of_a_writer_needs_its_worktree() {
        let dir = repo();
        let mut c = ctx(&dir);
        assert!(!run(&json!({"prompt": "w", "mode": "write"}), &mut c).is_error);
        let _ = std::fs::remove_dir_all(dir.join("session/subagents/wt-1/wt"));
        let out = run(&json!({"prompt": "more", "resume": "task-1"}), &mut c);
        assert!(out.is_error);
        assert!(
            out.text.contains("worktree gone — start a new task"),
            "{}",
            out.text
        );
    }

    #[test]
    fn read_escalates_once_one_tier_up() {
        let dir = tmpdir();
        let (mut c, mock) = ctx_with(
            &dir,
            vec![empty(), empty(), empty(), done_text("found it")],
            cfg(&dir),
        );
        let out = run(&json!({"prompt": "find"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.contains("found it"));
        assert!(
            out.text
                .contains("[escalated light→standard: empty response"),
            "{}",
            out.text
        );
        let f = Footer::parse(&out.text).unwrap();
        assert_eq!(
            (f.tier.as_str(), f.model.as_str()),
            ("standard", "claude-fable-5")
        );
        let r1 = sc(&dir, "task-1-r1");
        assert_eq!((r1.state, r1.tier), (State::Done, Tier::Standard));
        assert_eq!(
            sc(&dir, "task-1").escalated_to.as_deref(),
            Some("task-1-r1")
        );
        let seen = mock.seen.lock().unwrap();
        assert_eq!(seen.first().unwrap().model, "claude-haiku-4-5");
        assert_eq!(seen.last().unwrap().model, "claude-fable-5");
    }

    #[test]
    fn writers_never_escalate() {
        let dir = repo();
        let (mut c, _) = ctx_with(&dir, vec![empty()], cfg(&dir));
        let out = run(
            &json!({"prompt": "w", "mode": "write", "tier": "light"}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(Footer::parse(&out.text).unwrap().status, "empty");
        assert!(!dir.join("session/subagents/task-1-r1").exists());
    }

    fn pass_json() -> Response {
        done_text("ran tests\n```json\n{\"verdict\":\"pass\",\"evidence\":[],\"issues\":[],\"ran\":[\"true\"],\"confidence\":\"high\"}\n```")
    }

    #[test]
    fn verify_reviews_diff_and_untracked_and_parses_the_verdict() {
        let dir = repo();
        std::fs::write(dir.join("a.txt"), "two\n").unwrap();
        std::fs::write(dir.join("new.txt"), "brand new content\n").unwrap();
        let (mut c, mock) = ctx_with(&dir, vec![pass_json()], cfg(&dir));
        let out = run(
            &json!({"prompt": "a.txt says two", "mode": "verify"}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.starts_with("verdict: pass\n"), "{}", out.text);
        assert_eq!(
            Footer::parse(&out.text).unwrap().verdict.as_deref(),
            Some("pass")
        );
        let first = mock.seen.lock().unwrap()[0].clone();
        assert!(first.first_user.contains("+two"), "{}", first.first_user);
        assert!(first.first_user.contains("brand new content"));
        assert!(first.first_user.contains("[verifier contract]"));
        assert!(first.tools.iter().any(|t| t == "bash"));
        assert!(!first.tools.iter().any(|t| t == "write" || t == "task"));
    }

    #[test]
    fn background_verify_of_the_working_tree_is_refused() {
        let dir = repo();
        let (mut c, mock) = ctx_with(&dir, vec![pass_json()], cfg(&dir));
        let out = run(
            &json!({"prompt": "check", "mode": "verify", "background": true}),
            &mut c,
        );
        assert!(out.is_error);
        assert!(out.text.contains("needs a target"), "{}", out.text);
        assert!(mock.seen.lock().unwrap().is_empty(), "nothing ran");
        let acct = c.subagents.spend.as_ref().unwrap();
        assert_eq!(acct.reserved_usd(), 0.0, "nothing reserved");
        // Foreground stays allowed.
        let out = run(&json!({"prompt": "check", "mode": "verify"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        // A background resume of it would review the same tree.
        let out = run(
            &json!({"prompt": "again", "resume": "task-1", "background": true}),
            &mut c,
        );
        assert!(out.text.contains("needs a target"), "{}", out.text);
    }

    #[test]
    fn verifier_that_writes_fails() {
        let dir = repo();
        let (mut c, _) = ctx_with(
            &dir,
            vec![
                call(
                    1,
                    "bash",
                    json!({"command": "echo x > tampered.txt"}),
                    Usage::default(),
                ),
                pass_json(),
            ],
            cfg(&dir),
        );
        let out = run(&json!({"prompt": "check", "mode": "verify"}), &mut c);
        assert!(out.text.starts_with("verdict: fail"), "{}", out.text);
        assert!(
            out.text.contains("tampered: files tampered.txt"),
            "{}",
            out.text
        );
    }

    #[test]
    fn write_with_verify_chains_a_verifier_on_its_worktree() {
        let dir = repo();
        let (mut c, _) = ctx_with(
            &dir,
            vec![
                call(
                    1,
                    "bash",
                    json!({"command": "echo made > made.txt"}),
                    Usage::default(),
                ),
                done_text("digest body"),
            ],
            cfg(&dir),
        );
        let out = run(
            &json!({"prompt": "w", "mode": "write", "verify": true}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        // "digest body" has no JSON block: unknown, never a pass.
        assert!(
            out.text.contains("[verify task-2] verdict: unknown"),
            "{}",
            out.text
        );
        let f = Footer::parse(&out.text).unwrap();
        assert_eq!(
            (f.id.as_str(), f.verdict.as_deref()),
            ("task-1", Some("unknown"))
        );
        let v = sc(&dir, "task-2");
        assert_eq!((v.mode, v.state), (TaskMode::Verify, State::Done));
        assert_eq!(v.worktree, sc(&dir, "task-1").worktree);
        // An explicit target resolves through the writer's sidecar.
        let out = run(
            &json!({"prompt": "again", "mode": "verify", "target": "task-1"}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(sc(&dir, "task-3").worktree, sc(&dir, "task-1").worktree);
        let out = run(
            &json!({"prompt": "x", "mode": "verify", "target": "task-2"}),
            &mut c,
        );
        assert!(
            out.is_error && out.text.contains("not write"),
            "{}",
            out.text
        );
    }

    /// A verify that cannot even build its brief still carries the
    /// uniform verdict line.
    #[test]
    fn verify_whose_brief_fails_reports_unknown() {
        let dir = repo();
        let (mut c, _) = ctx_with(
            &dir,
            vec![
                call(
                    1,
                    "bash",
                    json!({"command": "echo x > new.txt"}),
                    Usage::default(),
                ),
                done_text("digest body"),
            ],
            cfg(&dir),
        );
        let out = run(&json!({"prompt": "w", "mode": "write"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        let wdir = dir.join("session/subagents/task-1");
        let mut w = sc(&dir, "task-1");
        w.base = Some("no-such-ref".into());
        w.store(&wdir).unwrap();
        let out = run(
            &json!({"prompt": "check", "mode": "verify", "target": "task-1"}),
            &mut c,
        );
        assert!(out.text.starts_with("verdict: unknown\n"), "{}", out.text);
        assert!(out.text.contains("no-such-ref"), "{}", out.text);
        assert_eq!(
            Footer::parse(&out.text).unwrap().verdict.as_deref(),
            Some("unknown")
        );
    }

    /// A writer that changed nothing has nothing to verify: no verifier
    /// runs, no cap is drawn, and the digest says so.
    #[test]
    fn chained_verify_is_skipped_without_changes() {
        let dir = repo();
        let (mut c, mock) = ctx_with(&dir, vec![done_text("nothing to do")], cfg(&dir));
        let out = run(
            &json!({"prompt": "w", "mode": "write", "verify": true}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("[verify skipped: no changes]"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("[verify task-2]"), "{}", out.text);
        assert_eq!(mock.seen.lock().unwrap().len(), 1, "writer only");
        assert!(Sidecar::load(&dir.join("session/subagents/task-2")).is_none());
        assert_eq!(Footer::parse(&out.text).unwrap().verdict, None);
    }

    #[test]
    fn consult_is_one_heavy_call_with_a_trace() {
        let dir = tmpdir();
        let (mut c, mock) = ctx_with(&dir, vec![done_text("advice text")], cfg(&dir));
        let out = run(
            &json!({"prompt": "which design?", "mode": "consult"}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(out.text.starts_with("advice text"));
        let f = Footer::parse(&out.text).unwrap();
        assert_eq!((f.mode.as_str(), f.tier.as_str()), ("consult", "heavy"));
        let seen = mock.seen.lock().unwrap().clone();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].tools.is_empty());
        assert_eq!(seen[0].max_tokens, 2_000);
        assert_eq!(seen[0].effort, Some(crate::provider::Effort::High));
        let d = dir.join("session/subagents/task-1");
        let events = crate::event::EventLog::replay(d.join("events.jsonl")).unwrap();
        assert_eq!(events.len(), 3);
        assert_eq!(
            crate::ledger::Ledger::read_all(d.join("ledger.jsonl")).len(),
            1
        );
        assert_eq!(sc(&dir, "task-1").cap_usd, budget::CONSULT_CAP_USD);
        let out = run(
            &json!({"prompt": "q", "mode": "consult", "background": true}),
            &mut c,
        );
        assert!(out.is_error && out.text.contains("background is refused"));
    }

    /// A consult refused for its cap never ran; a bigger tier would only
    /// be dearer, so it must not escalate.
    #[test]
    fn consult_cap_refusal_does_not_escalate() {
        let dir = tmpdir();
        let (mut c, mock) = ctx_with(&dir, vec![done_text("advice")], cfg(&dir));
        // $0.01 can't buy 1,024 standard-tier output tokens.
        let out = run(
            &json!({"prompt": "q", "mode": "consult", "tier": "standard", "max_cost_usd": 0.01}),
            &mut c,
        );
        assert!(out.text.contains("above its $0.0100 cap"), "{}", out.text);
        assert!(!out.text.contains("escalated"), "{}", out.text);
        assert_eq!(Footer::parse(&out.text).unwrap().status, "refused");
        assert!(mock.seen.lock().unwrap().is_empty(), "no call made");
        assert!(!dir.join("session/subagents/task-1-r1").exists());
        assert_eq!(
            escalation_reason(&Err(Failure::CapRefused("x".into()))),
            None
        );
        assert_eq!(
            escalation_reason(&Err(Failure::Error("x".into()))),
            Some("run error")
        );
    }

    #[test]
    fn filtered_view_drops_stale_and_regulated_topics() {
        // P8-B accept: the quarantined subagent view applies the validity
        // window, the TTL column, the sensitivity ceiling, and the
        // governance column — pointers AND bodies agree.
        let dir = std::env::temp_dir().join(format!("overseer-submem-{}", uuid::Uuid::now_v7()));
        let mem = dir.join("memory");
        for layer in ["profile", "episodic", "semantic", "procedural"] {
            std::fs::create_dir_all(mem.join(layer)).unwrap();
        }
        std::fs::write(mem.join("semantic/ok.md"), "fine body\n").unwrap();
        std::fs::write(
            mem.join("semantic/expired.md"),
            "---\nvalid_to: 2000-01-01T00:00:00Z\n---\nstale\n",
        )
        .unwrap();
        std::fs::write(
            mem.join("semantic/reg.md"),
            "---\ngovernance: regulated\n---\ncompliance\n",
        )
        .unwrap();
        std::fs::write(
            mem.join("semantic/secret.md"),
            "---\nsensitivity: secret\n---\nhush\n",
        )
        .unwrap();
        std::fs::write(
            mem.join("INDEX.md"),
            "# Memory Index\n\nok.md — fine\nexpired.md — stale\nreg.md — compliance\nsecret.md — hush\n",
        )
        .unwrap();

        let dest = dir.join("filtered");
        let out = filtered_memory_dir(&mem, crate::memory::Sensitivity::Personal, &dest)
            .expect("filtered dir");
        let index = std::fs::read_to_string(out.join("INDEX.md")).unwrap();
        assert!(index.contains("ok.md"), "{index}");
        assert!(!index.contains("expired.md"), "{index}");
        assert!(!index.contains("reg.md"), "{index}");
        assert!(!index.contains("secret.md"), "{index}");
        assert!(out.join("semantic/ok.md").exists(), "admitted body copied");
        for dropped in [
            "semantic/expired.md",
            "semantic/reg.md",
            "semantic/secret.md",
        ] {
            assert!(!out.join(dropped).exists(), "{dropped} must not be copied");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn filtered_view_keeps_layer_qualified_pointers() {
        // `semantic/x.md` in the INDEX is a pointer too — the file must
        // be materialized and the line kept; a qualified REGULATED
        // topic still drops out entirely.
        let dir = std::env::temp_dir().join(format!("overseer-submemq-{}", uuid::Uuid::now_v7()));
        let mem = dir.join("memory");
        std::fs::create_dir_all(mem.join("semantic")).unwrap();
        std::fs::write(mem.join("semantic/prefs.md"), "prefers tabs\n").unwrap();
        std::fs::write(
            mem.join("semantic/secret.md"),
            "---\nsensitivity: secret\n---\nhush\n",
        )
        .unwrap();
        std::fs::write(
            mem.join("INDEX.md"),
            "# Memory Index\n\nsemantic/prefs.md — tabs\nsemantic/secret.md — hush\n",
        )
        .unwrap();

        let dest = dir.join("filtered");
        let out = filtered_memory_dir(&mem, crate::memory::Sensitivity::Personal, &dest)
            .expect("filtered dir");
        let index = std::fs::read_to_string(out.join("INDEX.md")).unwrap();
        assert!(index.contains("semantic/prefs.md"), "{index}");
        assert!(!index.contains("semantic/secret.md"), "{index}");
        assert_eq!(
            std::fs::read_to_string(out.join("semantic/prefs.md")).unwrap(),
            "prefers tabs\n"
        );
        assert!(!out.join("semantic/secret.md").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn subagent_inherits_persona_draft_gate() {
        // F1: a read-mode subagent of a persona-gated parent denies draft reads.
        use serde_json::json;
        let dir = std::env::temp_dir().join(format!("overseer-subdraft-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(dir.join("persona")).unwrap();
        let persona = dir.join("persona");
        let parent_cfg = crate::agent::AgentConfig {
            cwd: dir.clone(),
            persona_dir: Some(persona.clone()),
            ..Default::default()
        };
        let ctx = ToolCtx {
            cwd: dir.clone(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: Some(parent_cfg),
            subagents: Default::default(),
            checkpoint: None,
            sandbox: false,
            broker: None,
        };
        let sub = sub_cfg(&ctx, &json!({}), &dir);
        let pol = if sub.full_access {
            crate::perm::Policy::allow_all()
        } else {
            let mut pol = crate::perm::Policy::preset(sub.policy_preset, dir.clone());
            pol.persona_dir = sub.persona_dir.clone();
            pol.persona_approved = sub
                .persona_dir
                .as_deref()
                .is_some_and(crate::onboard::all_approved);
            pol
        };
        let v = pol.check("read", &json!({"path": "persona/identity.md"}));
        assert!(
            matches!(v, crate::perm::Verdict::Deny { .. }),
            "subagent draft read must deny, got {v:?}"
        );
    }
}
