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
        other => {
            eprintln!("overseer: unknown command '{other}'");
            usage();
            2
        }
    }
}

fn usage() {
    eprintln!(
        "overseer {VERSION} — agentic coding engine\n\
         \n\
         USAGE:\n\
         \x20 overseer exec [FLAGS] <prompt>\n\
         \x20 overseer --version\n\
         \n\
         FLAGS (exec):\n\
         \x20 --json              Emit the event stream as JSONL on stdout\n\
         \x20 --resume <dir>      Resume an existing session directory\n\
         \x20 --session <dir>     Session directory (default: ~/.overseer/sessions/<ts>)\n\
         \x20 --cwd <dir>         Working directory for tools (default: .)\n\
         \x20 --model <id>        Model id (default: claude-sonnet-5)\n\
         \x20 --max-steps <n>     Step budget (default: 100)\n\
         \x20 --max-cost <usd>    Cost budget in USD (default: 5.0)\n\
         \x20 --thinking <tok>    Enable extended thinking with token budget\n\
         \n\
         ENV:\n\
         \x20 ANTHROPIC_API_KEY   Required for model calls"
    );
}

struct ExecFlags {
    json: bool,
    resume: Option<PathBuf>,
    session: Option<PathBuf>,
    cwd: PathBuf,
    model: String,
    max_steps: u32,
    max_cost: f64,
    thinking: Option<u32>,
    prompt: Option<String>,
}

fn parse_exec(args: &[String]) -> Result<ExecFlags, String> {
    let mut f = ExecFlags {
        json: false,
        resume: None,
        session: None,
        cwd: std::env::current_dir().map_err(|e| e.to_string())?,
        model: "claude-sonnet-5".into(),
        max_steps: 100,
        max_cost: 5.0,
        thinking: None,
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
            "--max-steps" => f.max_steps = take(&mut i)?.parse().map_err(|_| "bad --max-steps")?,
            "--max-cost" => f.max_cost = take(&mut i)?.parse().map_err(|_| "bad --max-cost")?,
            "--thinking" => f.thinking = Some(take(&mut i)?.parse().map_err(|_| "bad --thinking")?),
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

    let api_key = match std::env::var("ANTHROPIC_API_KEY") {
        Ok(k) if !k.is_empty() => k,
        _ => {
            eprintln!("overseer exec: ANTHROPIC_API_KEY is not set");
            return 2;
        }
    };
    let provider = Anthropic::new(api_key);
    let config = AgentConfig {
        model: flags.model.clone(),
        max_steps: flags.max_steps,
        max_cost_usd: flags.max_cost,
        max_output_tokens: 16_384,
        thinking_budget: flags.thinking,
        cwd: flags.cwd.clone(),
    };

    let mut agent = if flags.resume.is_some() {
        match Agent::resume(&provider, config, session_dir.clone()) {
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
        match Agent::start(&provider, config, session_dir.clone(), session_id) {
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
        EventKind::ToolResult { is_error, .. } if *is_error => {
            eprintln!("  ✗ tool error");
        }
        _ => {}
    }
}

fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".overseer")
}
