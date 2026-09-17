//! overseer — CLI entrypoint.
//!
//! Phase 0 surface (playbook Ch.2 §2.8, Ch.12 §0.3):
//!   overseer exec "task"                 run one prompt headlessly
//!   overseer exec --json "task"          emit the event stream as JSONL
//!   overseer exec --resume <dir> "next"  continue an existing session
//!   overseer --version                   fast path, <10ms (no deps loaded)
//!
//! Headless and TUI share the same engine — this file is the CI surface.

use std::io::Write;
use std::path::{Path, PathBuf};

use overseer_core::agent::{Agent, AgentConfig, RunOutcome};
use overseer_core::event::{Event, EventKind};
use overseer_core::provider::anthropic::Anthropic;
use overseer_core::provider::openai::OpenAiCompatible;
use overseer_core::provider::Provider;

const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Fast paths first — no provider init, no env probing (Ch.10 §2.2:
    // --version/--help must be instant).
    match args.first().map(String::as_str) {
        Some("--version") | Some("-V") | Some("version") => {
            println!("overseer {VERSION}");
            return 0;
        }
        Some("--help") | Some("-h") => {
            usage();
            return 0;
        }
        // Bare `overseer` is the TUI (Codex model: one binary, interactive
        // by default, `exec` for headless).
        None => return cmd_tui(&[]),
        _ => {}
    }

    // `overseer --flags` — flags with no subcommand go to the TUI.
    if args[0].starts_with('-') {
        return cmd_tui(&args);
    }

    match args[0].as_str() {
        "tui" => cmd_tui(&args[1..]),
        "exec" => cmd_exec(&args[1..]),
        "onboard" => cmd_onboard(&args[1..]),
        "consolidate" => cmd_consolidate(&args[1..]),
        "stats" => cmd_stats(&args[1..]),
        "rewind" => cmd_rewind(&args[1..]),
        "daemon" => cmd_daemon(&args[1..]),
        "inbox" => cmd_inbox(&args[1..]),
        "trigger" => cmd_trigger(&args[1..]),
        other => {
            eprintln!("overseer: unknown command '{other}'");
            usage();
            2
        }
    }
}

/// `overseer tui [exec flags]` / bare `overseer` — the interactive
/// terminal frontend. Same engine, same event stream, same flags as
/// `exec` (minus --json and the positional prompt).
fn cmd_tui(args: &[String]) -> i32 {
    // --no-tui: line mode — same session plumbing, plain-text REPL
    // (screen readers, terminals the inline viewport can't drive).
    let line_mode = args.iter().any(|a| a == "--no-tui");
    let args: Vec<String> = args.iter().filter(|a| *a != "--no-tui").cloned().collect();
    let flags = match parse_exec(&args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer tui: {e}");
            return 2;
        }
    };
    if flags.prompt.is_some() {
        eprintln!("overseer tui: no positional prompt — type inside the session");
        return 2;
    }
    if !line_mode && !std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        eprintln!("overseer: stdout is not a terminal — use `overseer exec` for pipes/CI");
        return 2;
    }
    let provider: std::sync::Arc<dyn Provider> = match build_provider(&flags) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer: {msg}");
            return 2;
        }
    };
    let (session_dir, resume) = resolve_session(&flags);
    let mut config = agent_config(&flags);
    apply_credentials(&mut config);
    let cfg = overseer_tui::TuiConfig {
        provider,
        agent: config,
        session_dir,
        resume,
    };
    match if line_mode {
        overseer_tui::run_line(cfg)
    } else {
        overseer_tui::run(cfg)
    } {
        Ok(code) => code,
        Err(e) => {
            eprintln!("overseer tui: {e}");
            1
        }
    }
}

/// Session directory + whether to resume it. Precedence: --resume >
/// --continue (cwd-scoped) > --last > --session > fresh timestamped dir.
/// `--bare` never lands in ~/.overseer — the session is a throwaway.
fn resolve_session(flags: &ExecFlags) -> (PathBuf, bool) {
    if flags.bare {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        return (
            std::env::temp_dir().join(format!("overseer-bare-{ts}")),
            false,
        );
    }
    if let Some(d) = &flags.resume {
        return (d.clone(), true);
    }
    let root = dirs_home().join("sessions");
    let cwd = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    if flags.cont {
        if let Some(d) = overseer_core::session::most_recent(&root, Some(&cwd)) {
            return (d, true);
        }
        eprintln!(
            "overseer: no earlier session for {} — starting fresh",
            cwd.display()
        );
    }
    if flags.last {
        if let Some(d) = overseer_core::session::most_recent(&root, None) {
            return (d, true);
        }
    }
    if let Some(d) = &flags.session {
        return (d.clone(), false);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    (dirs_home().join("sessions").join(format!("{ts}")), false)
}

fn agent_config(flags: &ExecFlags) -> overseer_core::agent::AgentConfig {
    let cwd_canonical = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    overseer_core::agent::AgentConfig {
        model: flags.model.clone(),
        max_steps: flags.max_steps,
        max_cost_usd: flags.max_cost,
        max_output_tokens: 16_384,
        thinking_budget: flags.thinking,
        effort: flags.effort,
        small_model: flags.small_model.clone(),
        // Canonicalize once: every subsystem (snapshots, read dedup, the
        // permission gate's containment check) assumes an absolute root —
        // a relative --cwd like "." would silently leak relative paths
        // into checkpoint manifests and policy checks.
        cwd: cwd_canonical.clone(),
        full_access: flags.full_access,
        policy_preset: flags.policy,
        auto_compact: flags.auto_compact,
        compact_at: flags.compact_at,
        memory_dir: flags.memory.then(|| cwd_canonical.join("memory")),
        // P6-2: the parent agent sees the full index; the ceiling applies
        // to the quarantined subagent view only.
        memory_filter: overseer_core::memory::Sensitivity::Personal,
        // P6-4: filled by `apply_credentials` (store resolution + payload).
        broker: overseer_core::cred::Broker::new(),
        keep_tool_results: flags.keep_results,
        verify_cmd: flags.verify.clone(),
        verify_block_cap: flags.verify_cap,
        sandbox_bash: flags.sandbox,
        disabled_tools: flags.no_tools.clone(),
        autonomy: {
            let mut m = std::collections::HashMap::new();
            for (d, l) in &flags.autonomy {
                let level = match l.as_str() {
                    "observe" => overseer_core::perm::Autonomy::Observe,
                    "suggest" => overseer_core::perm::Autonomy::Suggest,
                    "approve" => overseer_core::perm::Autonomy::ActWithApproval,
                    "report" => overseer_core::perm::Autonomy::ActAndReport,
                    _ => overseer_core::perm::Autonomy::ActSilently,
                };
                m.insert(d.clone(), level);
            }
            m
        },
        reflect: flags.reflect,
        credential_store: flags.credential_store,
        // P6-5: an existing <cwd>/persona dir joins the session — the
        // persona segment renders (approved bodies or the pending notice)
        // and the draft gate closes the dir to file tools until approved.
        // Same convention as --memory/<cwd>/memory.
        persona_dir: {
            let d = cwd_canonical.join("persona");
            d.is_dir().then_some(d)
        },
        ask_handler: None,
        // --bare: no persisted rules — a CI run must not inherit or
        // mutate the operator's allow-list. Ask verdicts still
        // fail-closed to deny either way.
        rules_path: if flags.bare {
            None
        } else {
            Some(dirs_home().join("rules"))
        },
    }
}

/// P6-4: resolve the credential store against this machine and load the
/// secrets it holds into the broker. The keychain read is prefetched so
/// its subprocess spawn overlaps the env read; the *effective* store is
/// written back onto the config, so `manifest.json` records what actually
/// held the secret (a fallback must be visible, not silent). Never prints
/// secret material — only the store and the fallback note.
fn apply_credentials(config: &mut overseer_core::agent::AgentConfig) {
    use overseer_core::cred;
    let configured = config.credential_store;
    let kc = cred::Keychain::detect();
    let prefetch = kc.prefetch();
    let env = cred::env_payload();
    let res = cred::resolve_store_with(configured, &kc, prefetch.resolve(), env);
    config.credential_store = res.store;
    if res.store != configured {
        eprintln!("overseer: credentials — {}", res.note);
    }
    let Some(text) = res.payload else {
        return;
    };
    match cred::parse_payload(&text) {
        Ok(p) if !p.is_empty() => {
            let (secrets, grants) = config.broker.install_payload(&p);
            if secrets > 0 || grants > 0 {
                eprintln!("overseer: credentials — {secrets} secret(s), {grants} grant(s) loaded");
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("overseer: credentials — {e} (ignored)"),
    }
}

/// `overseer onboard [--dir <d>] [--approve]` — guided persona onboarding
/// (P6-5). Without `--approve` it runs the interview over a real session,
/// records the answers in the event log (durable before anything is
/// written), then drafts the four persona files engine-side with
/// `<!-- source: answer-N -->` trailers. `--approve` trace-verifies and
/// flips every file to approved — the step that makes the persona visible
/// in the prompt and readable by tools.
fn cmd_onboard(args: &[String]) -> i32 {
    let flags = match parse_onboard(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer onboard: {e}");
            return 2;
        }
    };
    let dir = match flags.dir {
        Some(d) => d,
        None => std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join("persona"),
    };
    if let Err(e) = overseer_core::onboard::ensure_persona_dir(&dir) {
        eprintln!("overseer onboard: {} — {e}", dir.display());
        return 1;
    }

    if flags.approve {
        return match approve_persona(&dir) {
            Ok(msg) => {
                println!("onboard: {msg}");
                0
            }
            Err(e) => {
                eprintln!("overseer onboard: {e}");
                1
            }
        };
    }

    // The onboarding session uses the default provider/key resolution (the
    // `onboard` surface takes no provider flags by design).
    let mut base_flags = match parse_exec(&[]) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer onboard: {e}");
            return 2;
        }
    };
    let model = std::env::var("OVERSEER_ONBOARD_MODEL").unwrap_or_else(|_| "claude-sonnet-5".into());
    base_flags.model = model.clone();
    let provider: std::sync::Arc<dyn Provider> = match build_provider(&base_flags) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer onboard: {msg}");
            return 2;
        }
    };
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let session_dir = dirs_home().join("sessions").join(format!("onboard-{ts}"));
    let config = overseer_core::agent::AgentConfig {
        cwd: dir.parent().unwrap_or(&dir).to_path_buf(),
        model: model.clone(),
        // Drafting is engine-side; a model tool path into the draft dir
        // does not exist (and the gate would deny it anyway).
        persona_dir: Some(dir.clone()),
        disabled_tools: vec!["write".into(), "edit".into()],
        ..overseer_core::agent::AgentConfig::default()
    };
    let mut agent = match Agent::start(provider.clone(), config, session_dir.clone(), format!("onboard-{ts}")) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("overseer onboard: session {} — {e}", session_dir.display());
            return 1;
        }
    };

    let mut ask = |q: &str| -> Option<String> {
        println!("\n{q}\n");
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match std::io::stdin().read_line(&mut line) {
            Ok(0) | Err(_) => None,
            Ok(_) => Some(line),
        }
    };
    let mut sink = |_: &overseer_core::event::Event| {};
    let interview = match overseer_core::onboard::run_interview(&mut agent, &mut ask, &mut sink) {
        Ok(iv) => iv,
        Err(e) => {
            eprintln!("overseer onboard: {e}");
            return 1;
        }
    };
    println!(
        "onboard: {} answer(s) recorded in {}",
        interview.answers.len(),
        session_dir.display()
    );
    if interview.aborted {
        eprintln!("onboard: interview incomplete — answers kept; re-run to continue");
    }
    if interview.answers.is_empty() {
        return 1;
    }
    match overseer_core::onboard::draft_from_answers(
        provider.as_ref(),
        &model,
        &dir,
        &interview.answers,
    ) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// The `--approve` step (P6-5): trace-verify every insight against the
/// dir's own provenance claim, then flip the drafts to approved. Refuses on
/// orphans — an unsourced persona claim is exactly what the validator
/// exists to catch.
fn approve_persona(dir: &Path) -> Result<String, String> {
    let claimed = overseer_core::onboard::statuses(dir)
        .iter()
        .map(|(_, m)| m.source_answers)
        .max()
        .unwrap_or(0);
    if let Err(orphans) = overseer_core::onboard::verify_trace(dir, claimed) {
        return Err(format!(
            "refusing to approve — {} untraceable insight(s):\n  {}",
            orphans.len(),
            orphans.join("\n  ")
        ));
    }
    match overseer_core::onboard::approve(dir).map_err(|e| e.to_string())? {
        changed if changed.is_empty() => Ok(format!("already approved — {}", dir.display())),
        changed => Ok(format!(
            "approved {} file(s) in {}",
            changed.len(),
            dir.display()
        )),
    }
}

/// Parsed `overseer onboard` flags.
struct OnboardFlags {
    dir: Option<PathBuf>,
    approve: bool,
}

fn parse_onboard(args: &[String]) -> Result<OnboardFlags, String> {
    let mut f = OnboardFlags {
        dir: None,
        approve: false,
    };
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--dir" | "-d" => {
                i += 1;
                let v = args.get(i).ok_or("--dir needs a value")?;
                f.dir = Some(PathBuf::from(v));
            }
            "--approve" => f.approve = true,
            other => return Err(format!("unknown flag '{other}' (want --dir <d> | --approve)")),
        }
        i += 1;
    }
    Ok(f)
}

/// `overseer rewind <session-dir> [--checkpoint <n>] [--mode <m>]` —
/// P1.9 checkpoint rewind. Modes: `code` (restore snapshotted files),
/// `conversation` (truncate events at the checkpoint boundary),
/// `both` (default), `summarize` (truncate + compact what remains).
/// Blind spot: `bash` side effects are never snapshotted — only
/// write/edit edits are recorded in the manifest.
/// `overseer consolidate [--memory <dir>]` — sleep-time memory pass
/// (P3.8): small-tier dedupe of INDEX.md, git-committed.
fn cmd_consolidate(args: &[String]) -> i32 {
    let mut flags = match parse_exec(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer consolidate: {e}");
            return 2;
        }
    };
    flags.memory = true; // the command exists to touch memory
    let provider: std::sync::Arc<dyn Provider> = match build_provider(&flags) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer: {msg}");
            return 2;
        }
    };
    let dir = flags.cwd.join("memory");
    let model = flags
        .small_model
        .clone()
        .unwrap_or_else(|| flags.model.clone());
    match overseer_core::memory::consolidate(provider.as_ref(), &model, &dir) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("overseer consolidate: {e}");
            1
        }
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

fn cmd_rewind(args: &[String]) -> i32 {
    let mut dir: Option<PathBuf> = None;
    let mut want_cp: Option<u64> = None;
    let mut mode = "both".to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--checkpoint" => {
                i += 1;
                let Some(v) = args.get(i) else {
                    eprintln!("overseer rewind: --checkpoint needs a value");
                    return 2;
                };
                want_cp = v.trim_start_matches('e').parse().ok();
                if want_cp.is_none() {
                    eprintln!("overseer rewind: bad --checkpoint '{v}'");
                    return 2;
                }
            }
            "--mode" => {
                i += 1;
                match args.get(i).map(|s| s.as_str()) {
                    Some(m @ ("code" | "conversation" | "both" | "summarize")) => {
                        mode = m.to_string()
                    }
                    other => {
                        eprintln!(
                            "overseer rewind: bad --mode {:?} (code|conversation|both|summarize)",
                            other
                        );
                        return 2;
                    }
                }
            }
            s if s.starts_with('-') => {
                eprintln!("overseer rewind: unknown flag '{s}'");
                return 2;
            }
            s => dir = Some(PathBuf::from(s)),
        }
        i += 1;
    }
    let Some(dir) = dir else {
        eprintln!("overseer rewind: session dir required");
        return 2;
    };

    use overseer_core::rewind::Mode;
    let m = Mode::parse(&mode).unwrap_or(Mode::Both);
    match overseer_core::rewind::restore(&dir, want_cp, m) {
        Ok(r) => {
            if matches!(m, Mode::Code | Mode::Both) {
                println!(
                    "code: restored {} file(s), removed {} created file(s)",
                    r.restored, r.deleted
                );
            }
            if !matches!(m, Mode::Code) {
                println!(
                    "conversation: truncated {} event(s) at boundary e{}",
                    r.truncated, r.boundary
                );
            }
            if let Some(a) = r.compaction_at {
                println!("conversation: appended compaction summary at e{a}");
            }
            0
        }
        Err(e) => {
            eprintln!("overseer rewind: {e}");
            1
        }
    }
}

/// `overseer stats <session-dir>` — the cache-hit-rate dashboard
/// (playbook P1.1: ≥90% in-session is the SEV target).
fn cmd_stats(args: &[String]) -> i32 {
    let Some(dir) = args.first() else {
        eprintln!("overseer stats: session dir required");
        return 2;
    };
    let records = overseer_core::ledger::Ledger::read_all(PathBuf::from(dir).join("ledger.jsonl"));
    if records.is_empty() {
        eprintln!("overseer stats: no ledger records in {dir}");
        return 1;
    }
    let s = overseer_core::ledger::Ledger::summarize(&records);
    println!("session: {dir}");
    println!("calls:        {}", s.calls);
    println!(
        "input tokens: {} (cache-read {})",
        s.input_tokens, s.cache_read_tokens
    );
    println!("output tokens:{}", s.output_tokens);
    println!(
        "cache hit:    {:.1}%{}",
        s.cache_hit_rate * 100.0,
        if s.cache_hit_rate >= 0.9 {
            "  (≥90% SEV target met)"
        } else {
            ""
        }
    );
    println!("cost:         ${:.4}", s.total_cost_usd);
    println!("latency:      {}ms total", s.latency_ms);
    0
}

fn usage() {
    eprintln!(
        "overseer {VERSION} — agentic coding engine\n\
         \n\
         USAGE:\n\
         \x20 overseer [tui] [FLAGS]          interactive TUI (bare `overseer`)\n\
         \x20 overseer tui --no-tui           line mode (screen readers, plain REPL)\n\
         \x20 overseer exec [FLAGS] <prompt>\n\
         \x20 overseer stats <session-dir>   ledger dashboard (tokens, cache-hit, cost)\n\
         \x20 overseer rewind <session-dir> [--checkpoint <n>] [--mode <m>]\n\
         \x20                             restore a checkpoint; m = code|\n\
         \x20                             conversation|both|summarize\n\
         \x20 overseer onboard [--dir <d>] [--approve]\n\
         \x20                             persona interview (P6-5); drafts the\n\
         \x20                             four persona files, --approve makes\n\
         \x20                             them visible and readable\n\
         \x20 overseer --version\n\
         \n\
         FLAGS (exec):\n\
         \x20 --json              Emit the event stream as JSONL on stdout\n\
         \x20 --bare              Hermetic CI mode: --json + throwaway\n\
         \x20                     session in temp dir + no persisted\n\
         \x20                     rules (mutually exclusive with resume\n\
         \x20                     flags)\n\
         \x20 --resume <dir>      Resume an existing session directory\n\
         \x20 --continue, -c      Resume the most recent session for this cwd\n\
         \x20 --last              Resume the most recent session anywhere\n\
         \x20 --session <dir>     Session directory (default: ~/.overseer/sessions/<ts>)\n\
         \x20 --cwd <dir>         Working directory for tools (default: .)\n\
         \x20 --model <id>        Model id (default: claude-sonnet-5)\n\
         \x20 --provider <name>   anthropic | openai | legionedge | gemini (default: anthropic)\n\
         \x20 --effort <level>    min | low | medium | high | max (default: medium)\n\
         \x20 --small-model <id>  small-tier model for aux calls (titles, consolidation)\n\
         \x20 --base-url <url>    API base URL for openai-compatible providers\n\
         \x20 --max-steps <n>     Step budget (default: 100)\n\
         \x20 --max-cost <usd>    Cost budget in USD (default: 5.0)\n\
         \x20 --thinking <tok>    Enable extended thinking with token budget\n\
         \x20 --full-access       Disable the permission gate (benchmarks/\n\
         \x20                     sandboxed envs only)\n\
         \x20 --policy <preset>   workspace (default) | readonly | plan\n\
         \x20 --compact-at <f>    Compaction trigger, fraction of context\n\
         \x20                     window (default: model profile's)\n\
         \x20 --no-compact        Disable context-engine compaction\n\
         \x20 --keep-results <n>  Recent tool results kept verbatim (default: 5,\n\
         \x20                     0 disables stale-result clearing)\n\
         \x20 --verify <cmd>      Definition-of-done check; blocks finish on\n\
         \x20                     failure (stop-hook gate)\n\
         \x20 --verify-cap <n>    Max consecutive verify blocks (default: 8)\n\
         \x20 --reflect <mode>    off | reflexion (default: reflexion)\n\
         \x20                     aux-tier self-critique on verify blocks\n\
         \x20 --best-of <n>       N parallel attempts in git worktrees (2-4);\n\
         \x20                     first attempt passing --verify wins\n\
         \x20 --no-sandbox        Run bash unsandboxed (default: sandbox-exec/\n\
         \x20                     bwrap wrapper when available)\n\
         \x20 --memory            Enable file memory at <cwd>/memory\n\
         \x20 --autonomy <d=l>    Per-domain autonomy, repeatable (P5-B):
\
         \x20                     domains internal|external|money|identity;
\
         \x20                     levels observe|suggest|approve|report|silent
\
         \x20 --no-tools <list>   Ablation: comma-separated tool names removed\n\
         \x20                     from the spec list and refused at dispatch\n\
         \x20 --credential-store <s>  env | keychain | auto (default: auto —\n\
         \x20                     keychain first, env fallback)\n\
         \n\
         ENV:\n\
         \x20 OVERSEER_API_KEY    Provider key (preferred, any provider)\n\
         \x20 ANTHROPIC_API_KEY   Anthropic key\n\
         \x20 OPENAI_API_KEY      OpenAI-compatible key\n\
         \x20 GOOGLE_API_KEY      Gemini key (GEMINI_API_KEY also works)\n\
         \x20 LEK_API_KEY         LegionEdge key (fallback)\n\
         \x20 OVERSEER_CREDENTIALS  credential payload for --credential-store env\n\
         \x20                     (`NAME=value` lines, `grant …` lines)"
    );
}

struct ExecFlags {
    json: bool,
    resume: Option<PathBuf>,
    session: Option<PathBuf>,
    /// `--continue`: resume the most recent session for the cwd.
    cont: bool,
    /// `--last`: resume the most recent session anywhere.
    last: bool,
    /// `--bare`: hermetic CI mode — JSONL out, fresh session in a temp
    /// dir, no persisted rules, resume flags rejected.
    bare: bool,
    cwd: PathBuf,
    model: String,
    provider: String,
    base_url: Option<String>,
    max_steps: u32,
    max_cost: f64,
    thinking: Option<u32>,
    effort: Option<overseer_core::provider::Effort>,
    small_model: Option<String>,
    full_access: bool,
    policy: overseer_core::perm::Preset,
    auto_compact: bool,
    compact_at: Option<f32>,
    keep_results: usize,
    verify: Option<String>,
    verify_cap: u32,
    reflect: overseer_core::agent::ReflectMode,
    /// `--best-of N`: N parallel attempts in isolated git worktrees;
    /// first attempt whose verify command exits 0 wins.
    best_of: u32,
    sandbox: bool,
    memory: bool,
    /// `--no-tools a,b,c` — P4.3 ablation: named tools are removed from the
    /// spec list and refused at dispatch.
    no_tools: Vec<String>,
    /// `--autonomy external=suggest` — P5-B per-domain autonomy overrides
    /// (repeatable). Domains: internal, external, money, identity. Levels:
    /// observe, suggest, approve (act-with-approval), report (act+report),
    /// silent (act-silently).
    autonomy: Vec<(String, String)>,
    /// `--credential-store env|keychain|auto` — P6-4: where secrets are
    /// read from. Auto (default) tries the OS keychain and falls back to
    /// the environment; the effective store lands in the manifest.
    credential_store: overseer_core::cred::CredentialStore,
    prompt: Option<String>,
}

fn parse_exec(args: &[String]) -> Result<ExecFlags, String> {
    let mut f = ExecFlags {
        json: false,
        resume: None,
        session: None,
        cont: false,
        last: false,
        bare: false,
        cwd: std::env::current_dir().map_err(|e| e.to_string())?,
        model: "claude-sonnet-5".into(),
        provider: "anthropic".into(),
        base_url: None,
        max_steps: 100,
        max_cost: 5.0,
        thinking: None,
        effort: None,
        small_model: None,
        full_access: false,
        policy: overseer_core::perm::Preset::WorkspaceWrite,
        auto_compact: true,
        compact_at: None,
        keep_results: 5,
        verify: None,
        verify_cap: 8,
        reflect: overseer_core::agent::ReflectMode::Reflexion,
        best_of: 0,
        sandbox: true,
        memory: false,
        no_tools: Vec::new(),
        autonomy: Vec::new(),
        credential_store: overseer_core::cred::CredentialStore::Auto,
        prompt: None,
    };
    let mut i = 0;
    while i < args.len() {
        let a = args[i].as_str();
        let take = |i: &mut usize| -> Result<&String, String> {
            *i += 1;
            args.get(*i)
                .ok_or_else(|| format!("flag '{a}' needs a value"))
        };
        match a {
            "--json" => f.json = true,
            "--bare" => {
                f.bare = true;
                f.json = true;
            }
            "--resume" => f.resume = Some(PathBuf::from(take(&mut i)?)),
            "--continue" | "-c" => f.cont = true,
            "--last" => f.last = true,
            "--session" => f.session = Some(PathBuf::from(take(&mut i)?)),
            "--cwd" => f.cwd = PathBuf::from(take(&mut i)?),
            "--model" => f.model = take(&mut i)?.clone(),
            "--provider" => f.provider = take(&mut i)?.clone(),
            "--base-url" => f.base_url = Some(take(&mut i)?.clone()),
            "--max-steps" => f.max_steps = take(&mut i)?.parse().map_err(|_| "bad --max-steps")?,
            "--max-cost" => f.max_cost = take(&mut i)?.parse().map_err(|_| "bad --max-cost")?,
            "--thinking" => f.thinking = Some(take(&mut i)?.parse().map_err(|_| "bad --thinking")?),
            "--effort" => {
                f.effort = Some(
                    overseer_core::provider::Effort::parse(take(&mut i)?)
                        .ok_or("bad --effort (min|low|medium|high|max)")?,
                )
            }
            "--small-model" => f.small_model = Some(take(&mut i)?.clone()),
            "--full-access" => f.full_access = true,
            "--policy" => {
                f.policy = match take(&mut i)?.as_str() {
                    "workspace" => overseer_core::perm::Preset::WorkspaceWrite,
                    "readonly" => overseer_core::perm::Preset::ReadOnly,
                    "plan" => overseer_core::perm::Preset::Plan,
                    other => {
                        return Err(format!("bad --policy '{other}' (workspace|readonly|plan)"))
                    }
                }
            }
            "--compact-at" => {
                f.compact_at = Some(take(&mut i)?.parse().map_err(|_| "bad --compact-at")?)
            }
            "--no-compact" => f.auto_compact = false,
            "--keep-results" => {
                f.keep_results = take(&mut i)?.parse().map_err(|_| "bad --keep-results")?
            }
            "--verify" => f.verify = Some(take(&mut i)?.clone()),
            "--best-of" => f.best_of = take(&mut i)?.parse().map_err(|_| "bad --best-of")?,
            "--verify-cap" => {
                f.verify_cap = take(&mut i)?.parse().map_err(|_| "bad --verify-cap")?
            }
            "--reflect" => {
                f.reflect = match take(&mut i)?.as_str() {
                    "off" => overseer_core::agent::ReflectMode::Off,
                    "reflexion" => overseer_core::agent::ReflectMode::Reflexion,
                    other => return Err(format!("bad --reflect '{other}' (off|reflexion)")),
                }
            }
            "--no-sandbox" => f.sandbox = false,
            "--memory" => f.memory = true,
            "--autonomy" => {
                let v = take(&mut i)?;
                let (domain, level) = v
                    .split_once('=')
                    .ok_or("bad --autonomy (want domain=level, e.g. external=suggest)")?;
                if !["internal", "external", "money", "identity"].contains(&domain) {
                    return Err(format!(
                        "bad --autonomy domain '{domain}' (internal|external|money|identity)"
                    ));
                }
                match level {
                    "observe" | "suggest" | "approve" | "report" | "silent" => {}
                    _ => {
                        return Err(format!(
                            "bad --autonomy level '{level}' (observe|suggest|approve|report|silent)"
                        ))
                    }
                }
                f.autonomy.push((domain.to_string(), level.to_string()));
            }
            "--credential-store" => {
                f.credential_store = overseer_core::cred::CredentialStore::parse(take(&mut i)?)
                    .map_err(|e| format!("bad --credential-store ({e})"))?
            }
            "--no-tools" => {
                let v = take(&mut i)?;
                for name in v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
                    if !overseer_core::tools::TOOL_NAMES.contains(&name) {
                        return Err(format!(
                            "unknown tool '{name}' (--no-tools expects one of: {})",
                            overseer_core::tools::TOOL_NAMES.join(",")
                        ));
                    }
                    f.no_tools.push(name.to_string());
                }
            }
            "-" => {
                // Read the prompt from stdin (CI-friendly).
                let mut buf = String::new();
                std::io::Read::read_to_string(&mut std::io::stdin(), &mut buf)
                    .map_err(|e| e.to_string())?;
                f.prompt = Some(buf);
            }
            s if s.starts_with('-') => return Err(format!("unknown flag '{s}'")),
            s => f.prompt = Some(s.to_string()),
        }
        i += 1;
    }
    Ok(f)
}

fn cmd_exec(args: &[String]) -> i32 {
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

    let provider: std::sync::Arc<dyn Provider> = match build_provider(&flags) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer exec: {msg}");
            return 2;
        }
    };
    let mut config = agent_config(&flags);
    apply_credentials(&mut config);

    if flags.best_of >= 2 {
        return run_best_of(&flags, provider, config, session_dir);
    }

    let mut agent = if resume {
        match Agent::resume(provider.clone(), config, session_dir.clone()) {
            Ok(a) => a,
            Err(e) => {
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
    let mut sink = |e: &Event| {
        if json_mode {
            let line = serde_json::to_string(e).unwrap_or_default();
            let mut h = stdout.lock();
            let _ = writeln!(h, "{line}");
            let _ = h.flush();
        } else {
            render_human(e);
        }
    };

    match agent.run_turn(&prompt, &mut sink) {
        Ok(RunOutcome::Completed { steps, cost_usd }) => {
            eprintln!(
                "done: {steps} step(s), ${cost_usd:.4} — session {}",
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

const LEGIONEDGE_URL: &str = "https://inference.legionedge.ai/v1";

/// Build the provider from flags + env. Key resolution order:
/// OVERSEER_API_KEY → provider-specific env → LEK_API_KEY.
fn build_provider(flags: &ExecFlags) -> Result<Box<dyn Provider>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let key = env("OVERSEER_API_KEY")
        .or_else(|| match flags.provider.as_str() {
            "anthropic" => env("ANTHROPIC_API_KEY"),
            "gemini" => env("GOOGLE_API_KEY").or_else(|| env("GEMINI_API_KEY")),
            _ => env("OPENAI_API_KEY"),
        })
        .or_else(|| env("LEK_API_KEY"))
        .ok_or_else(|| {
            format!(
                "no API key for provider '{}' — set OVERSEER_API_KEY \
                 (or ANTHROPIC_API_KEY / OPENAI_API_KEY / LEK_API_KEY)",
                flags.provider
            )
        })?;
    Ok(match flags.provider.as_str() {
        "anthropic" => Box::new(Anthropic::new(key)),
        "openai" => Box::new(OpenAiCompatible::new(
            key,
            flags
                .base_url
                .clone()
                .unwrap_or_else(|| "https://api.openai.com/v1".into()),
        )),
        "gemini" => Box::new(overseer_core::provider::gemini::Gemini::new(
            key,
            flags
                .base_url
                .clone()
                .unwrap_or_else(|| "https://generativelanguage.googleapis.com/v1beta".into()),
        )),
        "legionedge" => Box::new(OpenAiCompatible::new(
            key,
            flags
                .base_url
                .clone()
                .unwrap_or_else(|| LEGIONEDGE_URL.into()),
        )),
        other => {
            return Err(format!(
                "unknown provider '{other}' (anthropic|openai|legionedge)"
            ))
        }
    })
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".overseer")
}

// ---------- gateway daemon (playbook 12.7 §5.1–5.3) ----------

fn daemon_dirs(args: &[String]) -> overseer_gateway::config::DaemonDirs {
    let dir = args
        .windows(2)
        .find(|w| w[0] == "--dir")
        .map(|w| PathBuf::from(&w[1]))
        .unwrap_or_else(|| dirs_home().join("daemon"));
    overseer_gateway::config::DaemonDirs::new(dir)
}

/// Positional args with `--dir <value>` pairs removed — so
/// `daemon --dir $DD status` detects `status`, not `$DD`. Only `--dir`
/// takes a value on the daemon/inbox/trigger surface; every other flag
/// is boolean.
fn positionals(args: &[String]) -> Vec<&String> {
    let mut out = Vec::new();
    let mut skip_next = false;
    for a in args {
        if skip_next {
            skip_next = false;
            continue;
        }
        if a == "--dir" {
            skip_next = true;
            continue;
        }
        if a.starts_with('-') {
            continue;
        }
        out.push(a);
    }
    out
}

fn ctl_call(
    dirs: &overseer_gateway::config::DaemonDirs,
    req: overseer_gateway::ctl::CtlRequest,
) -> Result<overseer_gateway::ctl::CtlResponse, String> {
    overseer_gateway::ctl::call(&dirs.socket(), &req)
}

/// `overseer daemon` — run the always-on gateway in the foreground
/// (launchd/systemd supervision comes with the release packaging).
/// Subcommands status/kill/reload go through the unix socket.
fn cmd_daemon(args: &[String]) -> i32 {
    let dirs = daemon_dirs(args);
    use overseer_gateway::ctl::CtlRequest;
    let pos = positionals(args);
    match pos.first().map(|s| s.as_str()) {
        Some("status") => match ctl_call(&dirs, CtlRequest::Status) {
            Ok(r) => {
                println!("{}", serde_json::to_string_pretty(&r).unwrap_or_default());
                if r.ok {
                    0
                } else {
                    1
                }
            }
            Err(e) => {
                eprintln!("overseer daemon: {e}");
                1
            }
        },
        Some("kill") => match ctl_call(&dirs, CtlRequest::Kill) {
            Ok(r) if r.ok => {
                println!("daemon stopping");
                0
            }
            Ok(r) => {
                eprintln!("overseer daemon: {}", r.error.unwrap_or_default());
                1
            }
            Err(e) => {
                eprintln!("overseer daemon: {e}");
                1
            }
        },
        Some("reload") => match ctl_call(&dirs, CtlRequest::Reload) {
            Ok(r) if r.ok => {
                println!("config reloaded");
                0
            }
            Ok(r) => {
                eprintln!("overseer daemon: {}", r.error.unwrap_or_default());
                1
            }
            Err(e) => {
                eprintln!("overseer daemon: {e}");
                1
            }
        },
        Some(other) => {
            eprintln!("overseer daemon: unknown subcommand '{other}' (run|status|kill|reload)");
            2
        }
        // Bare `overseer daemon` = run in foreground.
        None => {
            let bin = overseer_gateway::daemon::overseer_binary();
            match overseer_gateway::daemon::Daemon::new(dirs, bin) {
                Ok(mut d) => {
                    eprintln!(
                        "overseer daemon: running (kill: `overseer daemon kill` or touch STOP)"
                    );
                    d.run()
                }
                Err(e) => {
                    eprintln!("overseer daemon: {e}");
                    1
                }
            }
        }
    }
}

/// `overseer inbox` — the Agent Inbox surface: list/decide/act.
fn cmd_inbox(args: &[String]) -> i32 {
    let dirs = daemon_dirs(args);
    use overseer_gateway::ctl::CtlRequest;
    let rest: Vec<&String> = positionals(args);
    let sub = rest.first().map(|s| s.as_str());
    let req = match sub {
        Some("list") | None => CtlRequest::InboxList,
        Some("approve") | Some("reject") => {
            let Some(id) = rest.get(1) else {
                eprintln!("overseer inbox {sub:?}: needs an item id");
                return 2;
            };
            CtlRequest::InboxDecide {
                id: id.to_string(),
                decision: sub.unwrap().to_string(),
                snooze_ms: None,
            }
        }
        Some("snooze") => {
            let Some(id) = rest.get(1) else {
                eprintln!("overseer inbox snooze: needs an item id");
                return 2;
            };
            let ms = rest.get(2).and_then(|s| s.parse::<u64>().ok()).map(|v| {
                if v < 10_000 {
                    v * 1000
                } else {
                    v
                }
            });
            CtlRequest::InboxDecide {
                id: id.to_string(),
                decision: "snooze".into(),
                snooze_ms: ms,
            }
        }
        Some("act") => {
            let Some(id) = rest.get(1) else {
                eprintln!("overseer inbox act: needs an item id");
                return 2;
            };
            CtlRequest::InboxAct { id: id.to_string() }
        }
        Some(other) => {
            eprintln!(
                "overseer inbox: unknown subcommand '{other}' (list|approve|reject|snooze|act)"
            );
            return 2;
        }
    };
    match ctl_call(&dirs, req) {
        Ok(r) if r.ok => {
            if let Some(d) = r.data {
                println!("{}", serde_json::to_string_pretty(&d).unwrap_or_default());
            }
            0
        }
        Ok(r) => {
            eprintln!("overseer inbox: {}", r.error.unwrap_or_default());
            1
        }
        Err(e) => {
            eprintln!("overseer inbox: {e}");
            1
        }
    }
}

/// `overseer trigger fire` — inject an event (testing + webhook shim).
fn cmd_trigger(args: &[String]) -> i32 {
    let dirs = daemon_dirs(args);
    if args.first().map(String::as_str) != Some("fire") {
        eprintln!("overseer trigger: only 'fire' is supported");
        return 2;
    }
    let val = |flag: &str| args.windows(2).find(|w| w[0] == flag).map(|w| w[1].clone());
    let (Some(source), Some(class), Some(payload)) =
        (val("--source"), val("--class"), val("--payload"))
    else {
        eprintln!("overseer trigger fire: needs --source --class --payload");
        return 2;
    };
    let req = overseer_gateway::ctl::CtlRequest::TriggerFire {
        source,
        class,
        payload,
    };
    match ctl_call(&dirs, req) {
        Ok(r) if r.ok => {
            println!("fired");
            0
        }
        Ok(r) => {
            eprintln!("overseer trigger: {}", r.error.unwrap_or_default());
            1
        }
        Err(e) => {
            eprintln!("overseer trigger: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_implies_json_and_hermetic_session() {
        let f = parse_exec(&["--bare".into(), "do it".into()]).unwrap();
        assert!(f.bare);
        assert!(f.json, "--bare must imply --json");
        let (dir, resume) = resolve_session(&f);
        assert!(!resume, "--bare never resumes");
        // Throwaway session — never under ~/.overseer.
        assert!(
            dir.starts_with(std::env::temp_dir()),
            "bare session must live in temp: {}",
            dir.display()
        );
    }

    #[test]
    fn bare_still_enforces_sandbox_and_omits_rules() {
        let f = parse_exec(&["--bare".into(), "x".into()]).unwrap();
        let cfg = agent_config(&f);
        assert!(cfg.sandbox_bash, "--bare must not weaken the sandbox");
        assert!(cfg.rules_path.is_none(), "--bare loads no user rules");
    }
}

#[cfg(test)]
mod daemon_arg_tests {
    use super::*;

    #[test]
    fn daemon_dir_flag_order_independent() {
        // The live-smoke bug: `daemon --dir $DD status` read `$DD` as the
        // subcommand. --dir pairs strip before subcommand detection.
        assert_eq!(
            positionals(&["--dir".into(), "/tmp/x".into(), "status".into()]),
            vec!["status"]
        );
        assert_eq!(
            positionals(&["status".into(), "--dir".into(), "/tmp/x".into()]),
            vec!["status"]
        );
        assert!(positionals(&["--dir".into(), "/tmp/x".into()]).is_empty());
        assert_eq!(
            positionals(&[
                "approve".into(),
                "--dir".into(),
                "/tmp/x".into(),
                "abc".into()
            ]),
            vec!["approve", "abc"]
        );
    }
}

#[cfg(test)]
mod credential_flag_tests {
    use super::*;
    use overseer_core::cred::CredentialStore;

    #[test]
    fn credential_store_flag_parses_and_defaults_to_auto() {
        // Default: Auto — keychain first, env fallback.
        let f = parse_exec(&["x".into()]).unwrap();
        assert_eq!(f.credential_store, CredentialStore::Auto);
        assert_eq!(agent_config(&f).credential_store, CredentialStore::Auto);

        for (arg, want) in [
            ("env", CredentialStore::Env),
            ("keychain", CredentialStore::Keychain),
            ("auto", CredentialStore::Auto),
        ] {
            let f = parse_exec(&["--credential-store".into(), arg.into(), "x".into()]).unwrap();
            assert_eq!(f.credential_store, want);
            // The flag flows through to the run config (and the manifest).
            assert_eq!(agent_config(&f).credential_store, want);
        }
    }

    #[test]
    fn credential_store_flag_rejects_unknown_and_missing_value() {
        let e = parse_exec(&["--credential-store".into(), "bogus".into(), "x".into()])
            .err()
            .expect("bogus store must be rejected");
        assert!(e.contains("bad --credential-store"), "{e}");
        assert!(parse_exec(&["--credential-store".into()]).is_err());
    }

    /// P6-4: the resolved (effective) store is what the manifest records —
    /// a keychain fallback must not be reported as a keychain run.
    #[test]
    fn resolved_store_replaces_the_configured_one_on_fallback() {
        let flags =
            parse_exec(&["--credential-store".into(), "keychain".into(), "x".into()]).unwrap();
        let mut cfg = agent_config(&flags);
        assert_eq!(cfg.credential_store, CredentialStore::Keychain);
        let kc = overseer_core::cred::Keychain::new(
            Some("overseer-definitely-not-a-keychain-binary"),
            &["find-generic-password"],
            &["delete-generic-password"],
        );
        let res = overseer_core::cred::resolve_store_with(
            cfg.credential_store,
            &kc,
            kc.fetch(),
            Some("API_TOKEN=from-env".into()),
        );
        cfg.credential_store = res.store;
        assert_eq!(cfg.credential_store, CredentialStore::Env);
        // The env payload the fallback carried is usable as-is.
        let payload = overseer_core::cred::parse_payload(res.payload.as_deref().unwrap()).unwrap();
        assert_eq!(payload.secrets.len(), 1);
    }
}

#[cfg(test)]
mod onboard_flag_tests {
    use super::*;

    #[test]
    fn onboard_parses_dir_and_approve() {
        let f = parse_onboard(&[]).unwrap();
        assert!(f.dir.is_none(), "default dir is <cwd>/persona");
        assert!(!f.approve);
        let f = parse_onboard(&["--dir".into(), "/tmp/p".into(), "--approve".into()]).unwrap();
        assert_eq!(f.dir, Some(PathBuf::from("/tmp/p")));
        assert!(f.approve);
        // Order-independent, and the short form works.
        let f = parse_onboard(&["--approve".into(), "-d".into(), "p".into()]).unwrap();
        assert!(f.approve);
        assert_eq!(f.dir, Some(PathBuf::from("p")));
    }

    #[test]
    fn onboard_rejects_unknown_flags_and_missing_dir_value() {
        assert!(parse_onboard(&["--bogus".into()]).is_err());
        assert!(parse_onboard(&["--dir".into()]).is_err());
    }

    /// P6-5 wiring: a run whose cwd has a persona dir carries it on the
    /// config, so the prompt gate and the draft gate are live.
    #[test]
    fn exec_session_picks_up_an_existing_persona_dir() {
        let dir = std::env::temp_dir().join(format!("overseer-cli-persona-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = parse_exec(&["--cwd".into(), dir.to_string_lossy().to_string(), "x".into()]).unwrap();
        assert!(
            agent_config(&f).persona_dir.is_none(),
            "no persona dir → no gate armed"
        );
        overseer_core::onboard::ensure_persona_dir(&dir.join("persona")).unwrap();
        let cfg = agent_config(&f);
        let p = cfg
            .persona_dir
            .expect("an existing persona dir joins the session");
        assert_eq!(
            p.canonicalize().unwrap(),
            dir.join("persona").canonicalize().unwrap()
        );
        // Drafts by default → the dir is closed until approval.
        assert!(!overseer_core::onboard::all_approved(&p));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// P6-5 CLI accept: `--approve` refuses to unblind a persona whose
    /// insights don't trace, then approves once they do. Exercises the CLI's
    /// own gate (`approve_persona`), not just the library beneath it.
    #[test]
    fn approve_refuses_untraceable_persona() {
        let dir = std::env::temp_dir().join(format!("overseer-cli-onboard-{}", std::process::id()));
        let persona = dir.join("persona");
        let _ = std::fs::remove_dir_all(&dir);
        overseer_core::onboard::ensure_persona_dir(&persona).unwrap();
        overseer_core::onboard::write_drafts(
            &persona,
            &[(
                "identity.md".to_string(),
                vec![overseer_core::onboard::Insight {
                    text: "engineer".to_string(),
                    source: 4,
                }],
            )],
            2,
        )
        .unwrap();
        // An insight sourced from answer-4 when 2 were recorded: refused.
        let err = approve_persona(&persona).unwrap_err();
        assert!(err.contains("refusing to approve"), "{err}");
        assert!(err.contains("identity.md:"), "{err}");
        assert!(err.contains("out of range"), "{err}");
        assert!(overseer_core::onboard::statuses(&persona)
            .iter()
            .all(|(_, m)| m.status == overseer_core::onboard::Status::Draft));

        // Repair the source, then the same call approves every file.
        overseer_core::onboard::write_drafts(
            &persona,
            &[(
                "identity.md".to_string(),
                vec![overseer_core::onboard::Insight {
                    text: "engineer".to_string(),
                    source: 2,
                }],
            )],
            2,
        )
        .unwrap();
        let msg = approve_persona(&persona).unwrap();
        assert!(msg.contains("approved 4 file(s)"), "{msg}");
        assert!(overseer_core::onboard::all_approved(&persona));
        // Idempotent: a second approve reports the already-approved state.
        assert!(approve_persona(&persona).unwrap().contains("already approved"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod autonomy_flag_tests {
    use super::*;

    #[test]
    fn autonomy_flag_parses_domains_and_levels() {
        let f = parse_exec(&["--autonomy".into(), "external=suggest".into(), "x".into()]).unwrap();
        assert_eq!(
            f.autonomy,
            vec![("external".to_string(), "suggest".to_string())]
        );
        let f = parse_exec(&[
            "--autonomy".into(),
            "money=observe".into(),
            "--autonomy".into(),
            "identity=silent".into(),
            "x".into(),
        ])
        .unwrap();
        assert_eq!(f.autonomy.len(), 2);
    }

    #[test]
    fn autonomy_flag_rejects_bad_domain_and_level() {
        assert!(parse_exec(&["--autonomy".into(), "bogus=approve".into(), "x".into()]).is_err());
        assert!(parse_exec(&["--autonomy".into(), "external=bogus".into(), "x".into()]).is_err());
        assert!(parse_exec(&["--autonomy".into(), "external".into(), "x".into()]).is_err());
    }

    #[test]
    fn autonomy_flag_flows_into_agent_config() {
        let f = parse_exec(&["--autonomy".into(), "external=suggest".into(), "x".into()]).unwrap();
        let cfg = agent_config(&f);
        assert_eq!(
            cfg.autonomy.get("external"),
            Some(&overseer_core::perm::Autonomy::Suggest)
        );
    }
}
