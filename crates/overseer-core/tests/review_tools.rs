//! Hostile-review tests for the `tools` area (diff e059e02..cfc56a6).
//!
//! Every test is `#[ignore]`d: each one FAILS at cfc56a6 and pins a
//! confirmed finding. Run them with
//! `cargo test -p overseer-core --test review_tools -- --ignored`.

use overseer_core::perm::{AskDecision, AskHandler, Gate, Policy, Preset, Verdict};
use overseer_core::tools::{bash, ToolCtx};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("overseer-review-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

/// The unsandboxed exec path — `ctx.sandbox = false` is what `--no-sandbox`,
/// `--runtime native`, and the no-backend fallback all produce.
fn ctx(dir: &Path) -> ToolCtx<'static> {
    ToolCtx {
        cwd: dir.to_path_buf(),
        session_dir: dir.join("session"),
        spill_seq: 0,
        provider: None,
        agent_config: None,
        subagents: Default::default(),
        checkpoint: None,
        sandbox: false,
        broker: None,
    }
}

/// T1: `bash` hangs on a detached grandchild that holds the pipes.
///
/// `bash::run` reader threads `read_to_end` until EVERY writer of the
/// stdout/stderr pipes closes (bash.rs:92-101), and on timeout only the
/// direct `sh` pid is killed (bash.rs:109) — the process group is never
/// signalled, and there is no join grace. The TUI's `!` capture got
/// `process_group(0)` + `killpg` + JOIN_GRACE in this same diff
/// (app/shell.rs:83-120); the model-facing tool did not.
///
/// `(sleep 20 &)` exits `sh` immediately while an orphaned child keeps
/// the pipes — the tool call blocks for the grandchild's whole lifetime
/// and then still reports `exit 0`.
#[test]
#[ignore = "review: T1"]
fn bash_returns_when_sh_exits_with_a_detached_pipe_holding_grandchild() {
    let ws = tmpdir("bash-grandchild");
    let mut c = ctx(&ws);

    let start = Instant::now();
    let out = bash::run(&json!({ "command": "(sleep 20 &)" }), &mut c);
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "bash call blocked {elapsed:?} on an orphaned grandchild's pipes \
         (sh exits at once; the sleep holds stdout/stderr open)"
    );
    assert!(!out.is_error);

    // The timeout path is no better: `child.kill()` signals only the `sh`
    // pid — `sleep 15 & wait` orphans the sleeper at kill time and the
    // joins still wait for it.
    let start = Instant::now();
    let _ = bash::run(
        &json!({ "command": "sleep 15 & wait", "timeout_ms": 1000 }),
        &mut c,
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(10),
        "timeout_ms=1000 but the call blocked {elapsed:?} — no process-group kill"
    );
}

/// T2: `AllowSession`/`AllowAlways` record nothing for non-bash tools.
///
/// `gate` (perm.rs:1048-1074) only records a grant when
/// `session_key(tool, input)` returns `Some` — and `session_key`
/// (perm.rs:835-842) has a `"bash"` arm and `_ => None`, despite its own
/// doc saying "file tools key on the canonicalized path" and
/// `AskDecision::AllowSession` documenting "canonical file path". For
/// `write`, `edit`, `computer`, `memory`, `mcp` and `diagnostics`, the
/// human's "session"/"always" answer silently degrades to "once": the
/// identical call asks again. (The TUI still toasts
/// "rule saved to ~/.overseer/rules" — app/input.rs.)
#[test]
#[ignore = "review: T2"]
fn allow_session_is_recorded_for_non_bash_tools() {
    let root = tmpdir("session-grant");
    let mut p = Policy::preset(Preset::WorkspaceWrite, root.clone());
    let asks = Arc::new(AtomicUsize::new(0));
    let n = asks.clone();
    p.ask_handler = Some(AskHandler(Arc::new(
        move |_: &overseer_core::perm::AskRequest| {
            n.fetch_add(1, Ordering::SeqCst);
            AskDecision::AllowSession
        },
    )));
    // Arm the Rule-of-Two triangle so `write` asks before running.
    p.rearm("untrusted", "test");
    let _ = p.mark_sensitive("test");

    let input = json!({ "path": root.join("f.txt"), "content": "x" });
    assert!(
        matches!(p.gate("write", &input), Gate::Allow),
        "first call must pass on the human's AllowSession"
    );

    // The grant is supposed to stick: `check` on the identical call must
    // be Allow, not Ask again.
    assert!(
        matches!(p.check("write", &input), Verdict::Allow),
        "AllowSession recorded no grant for `write` — the identical call asks again"
    );
    assert_eq!(asks.load(Ordering::SeqCst), 1);
}

/// T3: a persisted `@turns=N` grant re-parses into a grant for a
/// different command — one the operator never approved.
///
/// `persist_rule` writes the raw `bash:<command>` key; `parse_line`
/// (perm.rs:602-624) then splits `line.split_once("@turns=")` on ANY
/// occurrence. A bash command that legitimately ends in ` @turns=N`
/// (operator approved the literal string) is persisted as
/// `bash:cmd @turns=N` and reloads as a TTL'd grant for `bash:cmd` — a
/// different, shorter command that was never approved — while the
/// approved command loses its own persistence. The retargeted grant
/// even shadows the Rule-of-Two Ask ("an explicit user grant beats the
/// latch", perm.rs:1492-1494).
#[test]
#[ignore = "review: T3"]
fn allow_always_does_not_retarget_a_grant_via_the_turns_suffix() {
    let dir = tmpdir("rules-turns");
    let rules = dir.join("rules");
    let root = tmpdir("rules-root");

    // Session 1 (taint armed so bash asks): operator says "always" to the
    // literal command `run @turns=500` — recorded and appended to rules.
    let mut p = Policy::preset(Preset::WorkspaceWrite, root.clone());
    p.ask_handler = Some(AskHandler(Arc::new(|_| AskDecision::AllowAlways)));
    p.load_rules(rules.clone());
    p.rearm("untrusted", "test");
    let _ = p.mark_sensitive("test");
    let approved = json!({ "command": "run @turns=500" });
    assert!(matches!(p.gate("bash", &approved), Gate::Allow));

    // Session 2 (reload, taint still armed): the persisted line
    // `bash:run @turns=500` parses as key `bash:run` + TTL — so `run`,
    // a command never approved, is allowed even under an armed
    // Rule-of-Two triangle.
    let mut p2 = Policy::preset(Preset::WorkspaceWrite, root);
    p2.load_rules(rules);
    p2.rearm("untrusted", "test");
    let _ = p2.mark_sensitive("test");
    assert!(
        matches!(
            p2.check("bash", &json!({ "command": "run" })),
            Verdict::Ask { .. }
        ),
        "grant for `run @turns=500` re-targeted onto `run` — \
         an unapproved command is allowed under an armed exfil triangle"
    );
}

/// T4: `open_append_no_follow` lacks the hard-link refusal.
///
/// `write_no_follow` refuses `nlink() > 1` on the open handle
/// (mod.rs:1207-1219) — "it may alias a file outside the workspace" —
/// and `open_append_no_follow`'s own doc claims "same refusal rule"
/// (mod.rs:1268-1269), but the nlink check was never ported
/// (mod.rs:1270-1297). The memory store's append paths
/// (`.index/uses.jsonl` via activation::record, the INDEX.md append)
/// write through a hard-linked journal file into its outside alias.
#[cfg(unix)]
#[test]
#[ignore = "review: T4"]
fn memory_journal_append_does_not_write_through_a_hardlink() {
    use overseer_core::memory::activation;

    let store = tmpdir("journal-store");
    std::fs::create_dir_all(store.join(".index")).unwrap();
    let outside = tmpdir("journal-outside").join("outside.txt");
    std::fs::write(&outside, "outside only\n").unwrap();
    std::fs::hard_link(&outside, store.join(activation::JOURNAL)).unwrap();

    let _ = activation::record(&store, "semantic/a.md", 1);
    assert_eq!(
        std::fs::read_to_string(&outside).unwrap(),
        "outside only\n",
        "journal append wrote through the hard link into the outside alias"
    );
}
