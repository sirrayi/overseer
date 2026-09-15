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
use std::path::PathBuf;

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
        "consolidate" => cmd_consolidate(&args[1..]),
        "stats" => cmd_stats(&args[1..]),
        "rewind" => cmd_rewind(&args[1..]),
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
    let config = agent_config(&flags);
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
        keep_tool_results: flags.keep_results,
        verify_cmd: flags.verify.clone(),
        verify_block_cap: flags.verify_cap,
        sandbox_bash: flags.sandbox,
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
        eprintln!("overseer exec: --best-of needs a git repo at {}", flags.cwd.display());
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
                    eprintln!("  attempt {j}: {}", if *ok { "passed (not first)" } else { "failed" });
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
         \x20 --provider <name>   anthropic | openai | fleet | gemini (default: anthropic)\n\
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
         \x20 --best-of <n>       N parallel attempts in git worktrees (2-4);\n\
         \x20                     first attempt passing --verify wins\n\
         \x20 --no-sandbox        Run bash unsandboxed (default: sandbox-exec/\n\
         \x20                     bwrap wrapper when available)\n\
         \x20 --memory            Enable file memory at <cwd>/memory\n\
         \n\
         ENV:\n\
         \x20 OVERSEER_API_KEY    Provider key (preferred, any provider)\n\
         \x20 ANTHROPIC_API_KEY   Anthropic key\n\
         \x20 OPENAI_API_KEY      OpenAI-compatible key\n\
         \x20 GOOGLE_API_KEY      Gemini key (GEMINI_API_KEY also works)\n\
         \x20 OVERSEER_API_KEY         Fleet key (fallback)"
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
    /// `--best-of N`: N parallel attempts in isolated git worktrees;
    /// first attempt whose verify command exits 0 wins.
    best_of: u32,
    sandbox: bool,
    memory: bool,
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
        best_of: 0,
        sandbox: true,
        memory: false,
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
            "--no-sandbox" => f.sandbox = false,
            "--memory" => f.memory = true,
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
        eprintln!(
            "overseer exec: --bare is hermetic — drop --resume/--continue/--last/--session"
        );
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
    let config = agent_config(&flags);

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

const FLEET_URL: &str = "https://inference.fleet.ai/v1";

/// Build the provider from flags + env. Key resolution order:
/// OVERSEER_API_KEY → provider-specific env → OVERSEER_API_KEY.
fn build_provider(flags: &ExecFlags) -> Result<Box<dyn Provider>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let key = env("OVERSEER_API_KEY")
        .or_else(|| match flags.provider.as_str() {
            "anthropic" => env("ANTHROPIC_API_KEY"),
            "gemini" => env("GOOGLE_API_KEY").or_else(|| env("GEMINI_API_KEY")),
            _ => env("OPENAI_API_KEY"),
        })
        .or_else(|| env("OVERSEER_API_KEY"))
        .ok_or_else(|| {
            format!(
                "no API key for provider '{}' — set OVERSEER_API_KEY \
                 (or ANTHROPIC_API_KEY / OPENAI_API_KEY / OVERSEER_API_KEY)",
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
        "fleet" => Box::new(OpenAiCompatible::new(
            key,
            flags
                .base_url
                .clone()
                .unwrap_or_else(|| FLEET_URL.into()),
        )),
        other => {
            return Err(format!(
                "unknown provider '{other}' (anthropic|openai|fleet)"
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
