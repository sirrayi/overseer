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
        Some("--help") | Some("-h") | None => {
            usage();
            return if args.is_empty() { 2 } else { 0 };
        }
        _ => {}
    }

    match args[0].as_str() {
        "exec" => cmd_exec(&args[1..]),
        "stats" => cmd_stats(&args[1..]),
        "rewind" => cmd_rewind(&args[1..]),
        other => {
            eprintln!("overseer: unknown command '{other}'");
            usage();
            2
        }
    }
}

/// `overseer rewind <session-dir> [--checkpoint <n>] [--mode <m>]` —
/// P1.9 checkpoint rewind. Modes: `code` (restore snapshotted files),
/// `conversation` (truncate events at the checkpoint boundary),
/// `both` (default), `summarize` (truncate + compact what remains).
/// Blind spot: `bash` side effects are never snapshotted — only
/// write/edit edits are recorded in the manifest.
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

    // Checkpoint dirs are named e<user-input-event-id>.
    let cp_root = dir.join("checkpoints");
    let mut cps: Vec<u64> = std::fs::read_dir(&cp_root)
        .map(|d| {
            d.filter_map(|e| {
                e.ok()?
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix('e')
                    .and_then(|n| n.parse().ok())
            })
            .collect()
        })
        .unwrap_or_default();
    cps.sort_unstable();
    let boundary = match want_cp.or_else(|| cps.last().copied()) {
        Some(b) if cps.contains(&b) => b,
        Some(b) => {
            eprintln!(
                "overseer rewind: no checkpoint e{b} in {}",
                cp_root.display()
            );
            return 1;
        }
        None => {
            eprintln!("overseer rewind: no checkpoints in {}", cp_root.display());
            return 1;
        }
    };
    let cp_dir = cp_root.join(format!("e{boundary}"));

    if mode == "code" || mode == "both" {
        let mut restored = 0u32;
        let mut deleted = 0u32;
        if let Ok(manifest) = std::fs::read_to_string(cp_dir.join("manifest.jsonl")) {
            for line in manifest.lines() {
                let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
                    continue;
                };
                let (Some(path), Some(stored)) = (
                    v.get("path").and_then(|p| p.as_str()),
                    v.get("stored").and_then(|s| s.as_str()),
                ) else {
                    continue;
                };
                if v.get("existed").and_then(|e| e.as_bool()).unwrap_or(false) {
                    let src = cp_dir.join("files").join(stored);
                    let dst = PathBuf::from(path);
                    if let Some(parent) = dst.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    if std::fs::copy(&src, &dst).is_ok() {
                        restored += 1;
                    }
                } else if std::fs::remove_file(path).is_ok() {
                    deleted += 1;
                }
            }
        }
        println!("code: restored {restored} file(s), removed {deleted} created file(s)");
    }

    if mode != "code" {
        // Truncate the log at the boundary (the user input that opened the
        // checkpoint stays; everything the agent did for it goes).
        let events_path = dir.join("events.jsonl");
        let Ok(text) = std::fs::read_to_string(&events_path) else {
            eprintln!("overseer rewind: cannot read {}", events_path.display());
            return 1;
        };
        let kept: Vec<&str> = text
            .lines()
            .filter(|l| {
                serde_json::from_str::<serde_json::Value>(l)
                    .ok()
                    .and_then(|v| v.get("id").and_then(|id| id.as_u64()))
                    .map(|id| id <= boundary)
                    .unwrap_or(true)
            })
            .collect();
        let dropped = text.lines().count() - kept.len();
        let tmp = events_path.with_extension("jsonl.tmp");
        if std::fs::write(&tmp, kept.join("\n") + "\n").is_err()
            || std::fs::rename(&tmp, &events_path).is_err()
        {
            eprintln!("overseer rewind: failed writing {}", events_path.display());
            return 1;
        }
        println!("conversation: truncated {dropped} event(s) at boundary e{boundary}");

        if mode == "summarize" {
            use overseer_core::compact;
            use overseer_core::event::{EventKind, EventLog};
            let events = EventLog::replay(&events_path).unwrap_or_default();
            if let Some(anchor) = compact::tail_anchor(&events, compact::TAIL_TURNS, 0) {
                let summary = compact::summarize(&events, anchor);
                match EventLog::open(&events_path) {
                    Ok(mut log) => {
                        if log
                            .append(EventKind::Compaction {
                                summary,
                                tail_from: anchor,
                            })
                            .and_then(|_| log.flush())
                            .is_ok()
                        {
                            println!("conversation: appended compaction summary at e{anchor}");
                        }
                    }
                    Err(e) => eprintln!("overseer rewind: compaction append failed: {e}"),
                }
            }
        }
    }
    0
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
         \x20 overseer exec [FLAGS] <prompt>\n\
         \x20 overseer stats <session-dir>   ledger dashboard (tokens, cache-hit, cost)\n\
         \x20 overseer rewind <session-dir> [--checkpoint <n>] [--mode <m>]\n\
         \x20                             restore a checkpoint; m = code|\n\
         \x20                             conversation|both|summarize\n\
         \x20 overseer --version\n\
         \n\
         FLAGS (exec):\n\
         \x20 --json              Emit the event stream as JSONL on stdout\n\
         \x20 --resume <dir>      Resume an existing session directory\n\
         \x20 --session <dir>     Session directory (default: ~/.overseer/sessions/<ts>)\n\
         \x20 --cwd <dir>         Working directory for tools (default: .)\n\
         \x20 --model <id>        Model id (default: claude-sonnet-5)\n\
         \x20 --provider <name>   anthropic | openai (default: anthropic)\n\
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
         \x20 --no-sandbox        Run bash unsandboxed (default: sandbox-exec/\n\
         \x20                     bwrap wrapper when available)\n\
         \x20 --memory            Enable file memory at <cwd>/memory\n\
         \n\
         ENV:\n\
         \x20 OVERSEER_API_KEY    Provider key (preferred, any provider)\n\
         \x20 ANTHROPIC_API_KEY   Anthropic key\n\
         \x20 OPENAI_API_KEY      OpenAI-compatible key"
    );
}

struct ExecFlags {
    json: bool,
    resume: Option<PathBuf>,
    session: Option<PathBuf>,
    cwd: PathBuf,
    model: String,
    provider: String,
    base_url: Option<String>,
    max_steps: u32,
    max_cost: f64,
    thinking: Option<u32>,
    full_access: bool,
    policy: overseer_core::perm::Preset,
    auto_compact: bool,
    compact_at: Option<f32>,
    keep_results: usize,
    verify: Option<String>,
    verify_cap: u32,
    sandbox: bool,
    memory: bool,
    prompt: Option<String>,
}

fn parse_exec(args: &[String]) -> Result<ExecFlags, String> {
    let mut f = ExecFlags {
        json: false,
        resume: None,
        session: None,
        cwd: std::env::current_dir().map_err(|e| e.to_string())?,
        model: "claude-sonnet-5".into(),
        provider: "anthropic".into(),
        base_url: None,
        max_steps: 100,
        max_cost: 5.0,
        thinking: None,
        full_access: false,
        policy: overseer_core::perm::Preset::WorkspaceWrite,
        auto_compact: true,
        compact_at: None,
        keep_results: 5,
        verify: None,
        verify_cap: 8,
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
            "--resume" => f.resume = Some(PathBuf::from(take(&mut i)?)),
            "--session" => f.session = Some(PathBuf::from(take(&mut i)?)),
            "--cwd" => f.cwd = PathBuf::from(take(&mut i)?),
            "--model" => f.model = take(&mut i)?.clone(),
            "--provider" => f.provider = take(&mut i)?.clone(),
            "--base-url" => f.base_url = Some(take(&mut i)?.clone()),
            "--max-steps" => f.max_steps = take(&mut i)?.parse().map_err(|_| "bad --max-steps")?,
            "--max-cost" => f.max_cost = take(&mut i)?.parse().map_err(|_| "bad --max-cost")?,
            "--thinking" => f.thinking = Some(take(&mut i)?.parse().map_err(|_| "bad --thinking")?),
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

    let session_dir = match (&flags.session, &flags.resume) {
        (Some(d), _) | (_, Some(d)) => d.clone(),
        _ => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0);
            dirs_home().join("sessions").join(format!("{ts}"))
        }
    };

    let provider: Box<dyn Provider> = match build_provider(&flags) {
        Ok(p) => p,
        Err(msg) => {
            eprintln!("overseer exec: {msg}");
            return 2;
        }
    };
    let cwd_canonical = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    let config = AgentConfig {
        model: flags.model.clone(),
        max_steps: flags.max_steps,
        max_cost_usd: flags.max_cost,
        max_output_tokens: 16_384,
        thinking_budget: flags.thinking,
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
    };

    let mut agent = if flags.resume.is_some() {
        match Agent::resume(provider.as_ref(), config, session_dir.clone()) {
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
        match Agent::start(provider.as_ref(), config, session_dir.clone(), session_id) {
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

/// Build the provider from flags + env. Key resolution order:
/// OVERSEER_API_KEY → provider-specific env.
fn build_provider(flags: &ExecFlags) -> Result<Box<dyn Provider>, String> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
    let key = env("OVERSEER_API_KEY")
        .or_else(|| match flags.provider.as_str() {
            "anthropic" => env("ANTHROPIC_API_KEY"),
            _ => env("OPENAI_API_KEY"),
        })
        .ok_or_else(|| {
            format!(
                "no API key for provider '{}' — set OVERSEER_API_KEY \
                 (or ANTHROPIC_API_KEY / OPENAI_API_KEY)",
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
        other => {
            return Err(format!(
                "unknown provider '{other}' (anthropic|openai)"
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
