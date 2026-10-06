//! `overseer exec` — one prompt, headless; the CI surface.

use std::io::Write;
use std::path::PathBuf;

use overseer_core::agent::{Agent, AgentConfig, RunOutcome};
use overseer_core::event::{Event, EventKind};
use overseer_core::provider::Provider;

use crate::flags::{parse_exec, ExecFlags};
use crate::provider::build_provider;
use crate::session::{agent_config, apply_credentials, resolve_session};

pub(crate) fn cmd_exec(args: &[String]) -> i32 {
    let flags = match parse_exec(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer exec: {e}");
            return 2;
        }
    };
    let prompt = match &flags.prompt {
        Some(p) => p.trim().to_string(),
        None => {
            eprintln!("overseer exec: a prompt is required (arg, or '-' for stdin)");
            return 2;
        }
    };
    if flags.bare && (flags.resume.is_some() || flags.cont || flags.last || flags.session.is_some())
    {
        eprintln!("overseer exec: --bare is hermetic — drop --resume/--continue/--last/--session");
        return 2;
    }

    let (session_dir, resume) = resolve_session(&flags);

    let mut config = agent_config(&flags);
    apply_credentials(&mut config);
    let provider: std::sync::Arc<dyn Provider> = match build_provider(&flags, &config.broker) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer exec: {msg}");
            return 2;
        }
    };

    if flags.best_of >= 2 {
        return run_best_of(&flags, provider, config, session_dir);
    }

    let mut agent = if resume {
        match Agent::resume(provider.clone(), config, session_dir.clone()) {
            Ok(a) => a,
            Err(e) => {
                // F4: a session opened by another process between
                // resolve_session and here — same refusal as the flag
                // pre-check (exit 2, same reason).
                if e.kind() == std::io::ErrorKind::WouldBlock {
                    eprintln!("overseer exec: {}", overseer_core::live::BUSY);
                    return 2;
                }
                eprintln!(
                    "overseer exec: cannot resume {}: {e}",
                    session_dir.display()
                );
                return 1;
            }
        }
    } else {
        let session_id = session_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".into());
        match Agent::start(provider.clone(), config, session_dir.clone(), session_id) {
            Ok(a) => a,
            Err(e) => {
                eprintln!("overseer exec: cannot start session: {e}");
                return 1;
            }
        }
    };

    let json_mode = flags.json;
    let stdout = std::io::stdout();
    // The run's cache delta rides its RunEnd; the final human summary
    // line appends the hit rate when the run billed input tokens.
    let mut run_cache = overseer_core::ledger::CacheStats::default();
    let mut sink = |e: &Event| {
        if let overseer_core::event::EventKind::RunEnd { cache, .. } = &e.kind {
            run_cache = *cache;
        }
        if json_mode {
            let line = serde_json::to_string(e).unwrap_or_default();
            let mut h = stdout.lock();
            let _ = writeln!(h, "{line}");
            let _ = h.flush();
        } else {
            render_human(e);
        }
    };

    let outcome = agent.run_turn(&prompt, &mut sink);
    let cache_tail = if run_cache.input_total() > 0 {
        format!(", {:.0}% cached", run_cache.hit_rate() * 100.0)
    } else {
        String::new()
    };
    match outcome {
        Ok(RunOutcome::Completed { steps, cost_usd }) => {
            eprintln!(
                "done: {steps} step(s), ${cost_usd:.4}{cache_tail} — session {}",
                session_dir.display()
            );
            0
        }
        Ok(RunOutcome::StepBudgetExceeded { steps, cost_usd }) => {
            eprintln!("step budget exceeded at {steps} steps (${cost_usd:.4})");
            3
        }
        Ok(RunOutcome::CostBudgetExceeded { steps, cost_usd }) => {
            eprintln!("cost budget exceeded at {steps} steps (${cost_usd:.4})");
            3
        }
        Ok(RunOutcome::Stuck { pattern, steps, .. }) => {
            eprintln!("run terminated — stuck: {pattern} ({steps} steps)");
            5
        }
        Ok(RunOutcome::EmptyResponse { steps, .. }) => {
            eprintln!("run terminated — model produced empty responses ({steps} steps)");
            6
        }
        Ok(RunOutcome::VerifyFailed { steps, .. }) => {
            eprintln!(
                "run terminated — verification still failing after block cap ({steps} steps)"
            );
            7
        }
        Ok(RunOutcome::Provider(msg)) => {
            eprintln!("provider error: {msg}");
            4
        }
        Ok(RunOutcome::Interrupted { steps, .. }) => {
            // Headless exec has no interrupter; reachable only if the
            // default Control were set — treat as a clean stop.
            eprintln!("run interrupted ({steps} steps)");
            130
        }
        Err(e) => {
            eprintln!("overseer exec: {e}");
            1
        }
    }
}

fn render_human(e: &Event) {
    match &e.kind {
        EventKind::ModelResponse {
            blocks, cost_usd, ..
        } => {
            for b in blocks {
                if let overseer_core::ir::Block::Text { text } = b {
                    println!("{text}");
                }
            }
            eprintln!("  [${cost_usd:.4}]");
        }
        EventKind::ToolCallStart { name, .. } => eprintln!("  → {name}"),
        EventKind::ToolResult {
            name,
            content,
            is_error,
            ..
        } => {
            if *is_error {
                eprintln!("  ✗ tool error");
            } else if name == "plan" {
                // Plan artifact stays user-visible in the transcript (P1.7).
                eprintln!("  {content}");
            }
        }
        _ => {}
    }
}

/// `--best-of N` (P3.9): N attempts run in parallel, each in its own git
/// worktree + session dir under the parent session. The winner is the
/// first attempt whose `--verify` command exits 0 in its worktree (with
/// no verify, the first clean completion wins). Attempt dirs persist for
/// audit; nothing is merged back automatically — the winner's branch
/// path is printed for review.
fn run_best_of(
    flags: &ExecFlags,
    provider: std::sync::Arc<dyn Provider>,
    base: AgentConfig,
    session_dir: PathBuf,
) -> i32 {
    let prompt = flags.prompt.clone().unwrap_or_default();
    // Worktrees need a repo; check before spawning anything.
    let repo = std::process::Command::new("git")
        .arg("-C")
        .arg(&flags.cwd)
        .args(["rev-parse", "--git-dir"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !repo {
        eprintln!(
            "overseer exec: --best-of needs a git repo at {}",
            flags.cwd.display()
        );
        return 2;
    }

    let n = flags.best_of.clamp(2, 4);
    let root = session_dir.join("bestof");
    let verify = flags.verify.clone();
    let mut handles = Vec::new();
    for i in 0..n {
        let provider = provider.clone();
        let mut cfg = base.clone();
        let wt = root.join(format!("attempt-{i}"));
        let sess = root.join(format!("sess-{i}"));
        let prompt = prompt.clone();
        let verify = verify.clone();
        let cwd = flags.cwd.clone();
        handles.push(std::thread::spawn(move || {
            let branch = format!("overseer-bon-{i}");
            let out = std::process::Command::new("git")
                .arg("-C")
                .arg(&cwd)
                .args(["worktree", "add", "-b", &branch])
                .arg(&wt)
                .output();
            match out {
                Ok(o) if o.status.success() => {}
                Ok(o) => {
                    return (
                        i,
                        false,
                        format!("worktree failed: {}", String::from_utf8_lossy(&o.stderr)),
                    );
                }
                Err(e) => return (i, false, format!("git: {e}")),
            }
            cfg.cwd = wt.clone();
            cfg.verify_cmd = None; // selection verifies explicitly below
            let id = format!("bon-{i}");
            let mut agent = match Agent::start(provider, cfg, sess.clone(), id) {
                Ok(a) => a,
                Err(e) => return (i, false, format!("start: {e}")),
            };
            let mut sink = |_: &Event| {};
            let outcome = agent.run_turn(&prompt, &mut sink);
            let answer = agent
                .messages()
                .last()
                .map(|m| m.text())
                .unwrap_or_default();
            let passed = match (&verify, &outcome) {
                (Some(v), Ok(_)) => std::process::Command::new("sh")
                    .arg("-c")
                    .arg(v)
                    .current_dir(&wt)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false),
                (None, Ok(_)) => true, // no gate ⇒ completion wins
                (_, Err(_)) => false,
            };
            let note = format!(
                "{}\n[attempt {i} — branch `{branch}`, trace {}, outcome {outcome:?}]",
                overseer_core::tools::middle_truncate(&answer, 8_000),
                sess.display()
            );
            (i, passed, note)
        }));
    }
    let mut winner: Option<(u32, String)> = None;
    let mut notes = Vec::new();
    for h in handles {
        let (i, passed, note) = h.join().unwrap_or((u32::MAX, false, "thread panic".into()));
        if passed && winner.is_none() {
            winner = Some((i, note.clone()));
        }
        notes.push((i, passed));
    }
    notes.sort();
    match winner {
        Some((i, note)) => {
            println!("{note}");
            for (j, ok) in &notes {
                if *j != i {
                    eprintln!(
                        "  attempt {j}: {}",
                        if *ok { "passed (not first)" } else { "failed" }
                    );
                }
            }
            0
        }
        None => {
            eprintln!("best-of-{n}: all attempts failed");
            for (i, ok) in &notes {
                eprintln!("  attempt {i}: {}", if *ok { "passed" } else { "failed" });
            }
            1
        }
    }
}
