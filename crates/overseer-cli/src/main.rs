//! overseer — CLI entrypoint: fast paths, dispatch and usage.
//!
//! Phase 0 surface (playbook Ch.2 §2.8, Ch.12 §0.3):
//!   overseer exec "task"                 run one prompt headlessly
//!   overseer exec --json "task"          emit the event stream as JSONL
//!   overseer exec --resume <dir> "next"  continue an existing session
//!   overseer --version                   fast path, <10ms (no deps loaded)
//!
//! Headless and TUI share the same engine. Each subcommand lives in
//! `cmd/<name>.rs` (`web` is `tui --web`, in cmd/tui.rs); every flag goes through the one parser in `args.rs`.

mod args;
mod cmd;
mod flags;
mod provider;
mod session;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

fn main() {
    let code = real_main();
    std::process::exit(code);
}

fn real_main() -> i32 {
    // Process posture before anything touches a key, the network, or the
    // filesystem: umask 0o077 + proxy-env scrub (harden.rs). Cheap enough
    // that even --version pays it without noticing.
    overseer_core::harden::harden_startup();
    // args_os + an explicit UTF-8 gate: `env::args()` panics on a
    // non-UTF-8 argument (I-non-utf8-argv) — a hostile argv must be a
    // usage error, never a crash.
    let mut args: Vec<String> = Vec::new();
    for (n, arg) in std::env::args_os().enumerate() {
        match arg.into_string() {
            Ok(s) => args.push(s),
            Err(_) => {
                eprintln!("overseer: argument {n} is not valid UTF-8");
                return 2;
            }
        }
    }
    args.remove(0);

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
        None => return cmd::tui::cmd_tui(&[]),
        _ => {}
    }

    // `overseer --flags` — flags with no subcommand go to the TUI.
    if args[0].starts_with('-') {
        return cmd::tui::cmd_tui(&args);
    }

    let rest = &args[1..];
    // `-h`/`--help` after any subcommand prints the same usage — the
    // shared arg parser would otherwise reject it as an unknown flag.
    if rest.iter().any(|a| a == "--help" || a == "-h") {
        usage();
        return 0;
    }
    match args[0].as_str() {
        "tui" => cmd::tui::cmd_tui(rest),
        "web" => cmd::tui::cmd_web(rest),
        "exec" => cmd::exec::cmd_exec(rest),
        "onboard" => cmd::onboard::cmd_onboard(rest),
        "consolidate" => cmd::consolidate::cmd_consolidate(rest),
        "stats" => cmd::stats::cmd_stats(rest),
        "mcp" => cmd::mcp::cmd_mcp(rest),
        "memory" => cmd::memory::cmd_memory(rest),
        "rewind" => cmd::rewind::cmd_rewind(rest),
        "daemon" => cmd::daemon::cmd_daemon(rest),
        "inbox" => cmd::inbox::cmd_inbox(rest),
        "trigger" => cmd::trigger::cmd_trigger(rest),
        "channel" => cmd::channel::cmd_channel(rest),
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
         \x20 overseer [tui] [FLAGS]          interactive TUI (bare `overseer`)\n\
         \x20 overseer tui --inline           live-strip surface (native scrollback)\n\
         \x20 overseer web [FLAGS] [--port <n>] [--no-open]\n\
         \x20                             browser surface on localhost; opens a tab\n\
         \x20                             (scans 8641+ unless --port pins one)\n\
         \x20                             alias: overseer tui --web [--web-port <n>]\n\
         \x20 overseer tui --no-tui           line mode (screen readers, plain REPL)\n\
         \x20 overseer exec [FLAGS] <prompt>  one prompt, headless ('-' reads stdin)\n\
         \x20 overseer onboard [--dir <d>] [--approve]\n\
         \x20                             persona interview (P6-5); drafts the\n\
         \x20                             four persona files, --approve makes\n\
         \x20                             them visible and readable\n\
         \x20 overseer consolidate [FLAGS]    sleep-time memory pass: dedupe INDEX.md,\n\
         \x20                             compact uses, distil new episodes\n\
         \x20                             (aux call on --small-model, else --model)\n\
         \x20 overseer memory where [FLAGS]  print the user and project store paths\n\
         \x20 overseer memory search <query> [FLAGS]\n\
         \x20                             ranked memory hits, as the memory tool\n\
         \x20 overseer memory pending | approve <id|all> | reject <id|all>\n\
         \x20                             staged review ops and proposals queue\n\
         \x20 overseer memory stats         notes per layer, pending, last review\n\
         \x20 overseer memory log [--store user|project]\n\
         \x20                             the store's git history, newest first\n\
         \x20 overseer memory restore <scope:layer/name.md>\n\
         \x20                             un-expire a forgotten note\n\
         \x20 overseer memory learn <session-dir> [--focus <text>]\n\
         \x20                             attended review over the session's\n\
         \x20                             unreviewed window (appends MemoryReview)\n\
         \x20 overseer stats <session-dir>    ledger dashboard (tokens, cache-hit, cost)\n\
         \x20 overseer mcp list               MCP servers from ~/.overseer/mcp.json:\n\
         \x20                             spawn each, print the tool names the\n\
         \x20                             mcp tool can call (exit 1 if any fail)\n\
         \x20 overseer rewind <session-dir> [--checkpoint <n>] [--mode <m>]\n\
         \x20                             restore a checkpoint; m = code|\n\
         \x20                             conversation|both|summarize\n\
         \x20 overseer daemon [status|kill|reload] [--dir <d>]\n\
         \x20                             always-on gateway (bare: run in the\n\
         \x20                             foreground; default dir ~/.overseer/daemon)\n\
         \x20 overseer inbox [list|approve <id>|reject <id>|snooze <id> [<s>]|act <id>]\n\
         \x20                             [--dir <d>]  the Agent Inbox\n\
         \x20 overseer trigger fire --source <s> --class <c> --payload <p> [--dir <d>]\n\
         \x20                             inject an event into the daemon\n\
         \x20 overseer channel send --to <d> --text <m> [--thread <t>] [--via <c>]\n\
         \x20 overseer channel digest\n\
         \x20 overseer channel signal [--focused <b>] [--dnd <b>] [--calendar-busy <b>]\n\
         \x20                         [--app <name>] [--idle <s>]  [--dir <d>]\n\
         \x20                             outbound drafts, digest, attention facts\n\
         \x20 overseer --version\n\
         \n\
         Every value flag accepts `--flag value` and `--flag=value`.\n\
         \n\
         FLAGS (exec, tui, consolidate, memory):\n\
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
         \x20 --provider <name>   anthropic | openai | opencode | gemini (default: anthropic)\n\
         \x20 --effort <level>    min | low | medium | high | max (default: medium)\n\
         \x20 --small-model <id>  small-tier model for aux calls (titles, consolidation)\n\
         \x20                    and light subagents\n\
         \x20 --heavy-model <id>  model for heavy subagents (consult, escalation)\n\
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
         \x20 --runtime <name>    Pin the bash sandbox backend (P8-C): native |\n\
         \x20                     seatbelt | bubblewrap | gvisor. An unavailable\n\
         \x20                     runtime fails the call instead of downgrading\n\
         \x20 --memory            Legacy project store at <cwd>/memory (file\n\
         \x20                     tools can write it); memory is otherwise on\n\
         \x20                     by default under $OVERSEER_HOME (~/.overseer)\n\
         \x20 --no-memory         No memory: no stores, recall or episodes\n\
         \x20 --no-learn          Disable the memory review pass entirely\n\
         \x20 --learn-every <n>   User-turn cadence for reviews (default: 6)\n\
         \x20 --learn-stage       Stage every review op for approval instead\n\
         \x20                     of applying (overseer memory pending/approve)\n\
         \x20 --autonomy <d=l>    Per-domain autonomy, repeatable (P5-B):\n\
         \x20                     domains internal|external|money|identity;\n\
         \x20                     levels observe|suggest|approve|report|silent\n\
         \x20 --no-tools <list>   Ablation: comma-separated tool names removed\n\
         \x20                     from the spec list and refused at dispatch\n\
         \x20 --credential-store <s>  env | keychain | auto (default: auto —\n\
         \x20                     keychain first, env fallback)\n\
         \n\
         FLAGS (tui only):\n\
         \x20 --no-tui            Line mode (plain-text REPL)\n\
         \x20 --inline            Live-strip surface in native scrollback\n\
         \x20 --web               Browser surface on localhost\n\
         \x20 --web-port <n>      Port for --web (absent: scan 8641-8660)\n\
         \x20 --no-open           Print the URL instead of opening a tab\n\
         \n\
         ENV (provider keys; each provider reads only its own name, from env\n\
         \x20    first, then the same name in the credential payload):\n\
         \x20 ANTHROPIC_API_KEY   Anthropic key\n\
         \x20 OPENAI_API_KEY      OpenAI-compatible key\n\
         \x20 GOOGLE_API_KEY      Gemini key (GEMINI_API_KEY also works)\n\
         \x20 OPENCODE_API_KEY    opencode Go key (provider=opencode)\n\
         \x20 OVERSEER_CREDENTIALS  credential payload for --credential-store env\n\
         \x20                     (`NAME=value` lines, `grant …` lines)"
    );
}
