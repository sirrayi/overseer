//! `overseer memory …` — inspect the memory stores, drive the pending
//! queue, and run an attended learning review (memory v3).

use std::path::{Path, PathBuf};

use overseer_core::event::{EventKind, EventLog};
use overseer_core::memory::{self, pending, Scope};
use overseer_core::provider::Provider;

use crate::args::{self, Arg, Flag};
use crate::flags::{exec_from, EXEC_FLAGS};
use crate::session::{agent_config, apply_credentials, memory_store_list};

/// The shared exec set plus the memory-only flags (`--store` selects one
/// store for `log`; `--focus` steers an attended `learn`).
fn memory_flags() -> Vec<Flag> {
    let mut f = EXEC_FLAGS.to_vec();
    f.push(Flag::value(&["--store"]));
    f.push(Flag::value(&["--focus"]));
    f
}

const USAGE: &str = "overseer memory: expected one of
  where                      resolved store paths
  search <query>             ranked hits (the memory tool's view)
  pending                    staged review ops + proposals
  approve <id|all>           apply a staged op (refuses a drifted target)
  reject <id|all>            drop a staged op
  stats                      notes, pending, last review per store
  log [--store user|project] git history, newest first
  restore <scope:layer/name.md>   un-expire a forgotten note
  learn <session-dir> [--focus <text>]   attended review over the session";

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

pub(crate) fn cmd_memory(argv: &[String]) -> i32 {
    let parsed = match args::parse(argv, &memory_flags()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("overseer memory: {e}");
            return 2;
        }
    };
    let words: Vec<String> = parsed.positionals().iter().map(|w| w.to_string()).collect();
    let store_arg = parsed.value("--store").map(str::to_string);
    let focus = parsed.value("--focus").map(str::to_string);
    let flag_args = parsed
        .args
        .into_iter()
        .filter(|a| matches!(a, Arg::Flag { name, .. } if *name != "--store" && *name != "--focus"))
        .collect();
    let flags = match exec_from(flag_args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer memory: {e}");
            return 2;
        }
    };
    let cwd = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    let stores = memory_store_list(&flags, &cwd);
    match words.first().map(String::as_str) {
        Some("where") if words.len() == 1 => {
            if stores.is_empty() {
                println!("memory: off");
            }
            for (scope, dir) in &stores {
                println!("{}: {}", scope.name(), dir.display());
            }
            0
        }
        Some("search") if words.len() > 1 => {
            let query = words[1..].join(" ");
            let now = now_secs();
            let idx = memory::index::Index::build(&stores, now);
            match overseer_core::tools::memory_tool::search_text(&idx, &query, now) {
                Some(out) => {
                    print!("{out}");
                    0
                }
                None => {
                    println!("no notes match `{query}`");
                    1
                }
            }
        }
        Some("pending") if words.len() == 1 => cmd_pending(&stores),
        Some("approve") | Some("reject") if words.len() == 2 => {
            let approve = words[0] == "approve";
            cmd_resolve(&stores, &words[1], approve)
        }
        Some("stats") if words.len() == 1 => cmd_stats(&stores),
        Some("log") if words.len() == 1 => cmd_log(&stores, store_arg.as_deref()),
        Some("restore") if words.len() == 2 => cmd_restore(&stores, &words[1]),
        Some("learn") if words.len() == 2 => {
            cmd_learn(&flags, Path::new(&words[1]), focus.as_deref())
        }
        _ => {
            eprintln!("{USAGE}");
            2
        }
    }
}

/// `overseer memory pending` — staged ops across every store, then v2
/// `proposals/` items (marked).
fn cmd_pending(stores: &[(Scope, PathBuf)]) -> i32 {
    let items = pending::list(stores);
    if items.is_empty() {
        println!("memory: nothing pending");
        return 0;
    }
    for it in items {
        let kind = if it.proposal {
            "proposal".to_string()
        } else {
            it.kind.clone()
        };
        println!(
            "{:<14} {:<7} {:<10} {:<24} {}",
            it.id,
            it.scope.name(),
            kind,
            it.target.unwrap_or_default(),
            it.summary
        );
    }
    0
}

/// `approve`/`reject <id|all>` — `all` walks the pending list once.
fn cmd_resolve(stores: &[(Scope, PathBuf)], arg: &str, approve: bool) -> i32 {
    let now = now_secs();
    let one = |id: &str| -> Result<String, String> {
        if approve {
            pending::approve(stores, id, now)
        } else {
            pending::reject(stores, id)
        }
    };
    if arg == "all" {
        let items = pending::list(stores);
        // Bulk-approve covers staged records only — a proposal carries
        // quarantined tainted text and always gets an individual look.
        // `reject all` may still drop them.
        let proposals = items.iter().filter(|i| i.proposal).count();
        let ids: Vec<String> = items
            .iter()
            .filter(|i| !approve || !i.proposal)
            .map(|i| i.id.clone())
            .collect();
        if ids.is_empty() {
            if approve && proposals > 0 {
                println!("skipped {proposals} proposals — approve them one by one by id");
            } else {
                println!("memory: nothing pending");
            }
            return 0;
        }
        let mut rc = 0;
        for id in ids {
            match one(&id) {
                Ok(msg) => println!("{msg}"),
                Err(e) => {
                    eprintln!("{e}");
                    rc = 1;
                }
            }
        }
        if approve && proposals > 0 {
            println!("skipped {proposals} proposals — approve them one by one by id");
        }
        return rc;
    }
    match one(arg) {
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

/// `overseer memory stats` (§8): live notes per store and layer, pending
/// and proposal counts, the last review and last dream, learned skills
/// (none until H2).
fn cmd_stats(stores: &[(Scope, PathBuf)]) -> i32 {
    if stores.is_empty() {
        println!("memory: off");
        return 0;
    }
    let now = now_secs();
    let idx = memory::index::Index::build(stores, now);
    let pend = pending::list(stores);
    for (scope, dir) in stores {
        println!("{}: {}", scope.name(), dir.display());
        for layer in memory::Layer::ALL {
            let n = idx
                .docs
                .iter()
                .filter(|d| d.scope == *scope && d.rel.starts_with(&format!("{}/", layer.name())))
                .count();
            println!("  {:<11} {}", layer.name(), n);
        }
        let staged = pend
            .iter()
            .filter(|p| p.scope == *scope && !p.proposal)
            .count();
        let proposals = pend
            .iter()
            .filter(|p| p.scope == *scope && p.proposal)
            .count();
        println!("  pending     {staged} (+{proposals} proposals)");
        let review = last_marker(&dir.join(".index/review.json"), "trigger");
        println!("  last review {}", review);
        let dream = last_marker(&dir.join(".index/dream.json"), "kind");
        println!("  last dream  {dream}");
    }
    // DEFERRED(owner): learned-skills rows by status — gate: §5/H2.
    println!("learned skills: 0 (learned skills land in H2)");
    0
}

/// `{"ts": …, "trigger": …}` → `"<ts> (<trigger>)"`; "never" when absent.
fn last_marker(path: &Path, tag: &str) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return "never".into();
    };
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
        return "never".into();
    };
    let ts = v.get("ts").and_then(|t| t.as_str()).unwrap_or("?");
    let tag = v.get(tag).and_then(|t| t.as_str()).unwrap_or("?");
    format!("{ts} ({tag})")
}

/// `overseer memory log [--store user|project]` — git --oneline per
/// store (or the named one), newest first, 50 lines.
fn cmd_log(stores: &[(Scope, PathBuf)], store: Option<&str>) -> i32 {
    let chosen: Vec<(Scope, PathBuf)> = match store {
        Some(name) => {
            let Some(scope) = Scope::parse(name) else {
                eprintln!("overseer memory log: bad --store `{name}` (user|project)");
                return 2;
            };
            stores
                .iter()
                .filter(|(s, _)| *s == scope)
                .cloned()
                .collect()
        }
        None => stores.to_vec(),
    };
    if chosen.is_empty() {
        println!("memory: off");
        return 0;
    }
    let mut any = false;
    for (scope, dir) in &chosen {
        let lines = memory::git_log(dir, 50);
        println!("{}:", scope.name());
        if lines.is_empty() {
            println!("  (no history)");
        }
        for l in lines {
            println!("  {l}");
            any = true;
        }
    }
    if !any && chosen.iter().all(|(_, d)| !d.join(".git").exists()) {
        // No store has history at all — still a success, just quiet.
    }
    0
}

/// `overseer memory restore <scope:layer/name.md>` — un-expire a note.
fn cmd_restore(stores: &[(Scope, PathBuf)], name: &str) -> i32 {
    let Some((scope, _layer, rel)) = memory::parse_qualified(name) else {
        eprintln!("overseer memory restore: want <scope:layer/name.md>, got `{name}`");
        return 2;
    };
    let Some((_, dir)) = stores.iter().find(|(s, _)| *s == scope) else {
        eprintln!("overseer memory restore: no {} store", scope.name());
        return 1;
    };
    match memory::restore(dir, &rel) {
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

/// The stores `learn` targets come from the SESSION's recorded
/// `SessionStart.cwd` — not the invoker's — so a learn run from another
/// directory can't write project A's lessons into project B's store.
/// Falls back to the invoking cwd when the log has no SessionStart.
fn session_stores(
    flags: &crate::flags::ExecFlags,
    events: &[overseer_core::event::Event],
) -> Vec<(Scope, PathBuf)> {
    let session_cwd = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SessionStart { cwd, .. } if !cwd.is_empty() => {
                let p = PathBuf::from(cwd);
                Some(p.canonicalize().unwrap_or(p))
            }
            _ => None,
        })
        .unwrap_or_else(|| flags.cwd.clone());
    memory_store_list(flags, &session_cwd)
}

/// `overseer memory learn <session-dir> [--focus <text>]` — an attended
/// review (§1.2 trigger 5) over the session's events since its review
/// cursor. Attended means SUPERSEDE/FORGET apply directly (§1.6).
///
/// The review IS recorded: `EventLog::open` replays the log to rebuild
/// the hash chain and `append` extends it — the same append-safety the
/// resume path relies on — so `MemoryReview` lands in the session's own
/// `events.jsonl` and the review call's usage row in its `ledger.jsonl`
/// (`purpose: memory_review`). Contract: one process writes a session
/// at a time — never run this against a session a live agent is writing.
fn cmd_learn(flags: &crate::flags::ExecFlags, session_dir: &Path, focus: Option<&str>) -> i32 {
    use overseer_core::memory::learn;
    // F4: this command appends a MemoryReview to the session's
    // events.jsonl — refusing on a live session protects the hash chain
    // from two writers minting the same next id. `held` is a read-only
    // probe (no file created), so it can run before the session is even
    // validated; acquire below is the authoritative check.
    if overseer_core::live::held(session_dir) {
        eprintln!(
            "overseer memory learn: {} — run learn after it exits",
            overseer_core::live::BUSY
        );
        return 1;
    }
    // Validate the session BEFORE the lock: acquire creates live.lock in
    // the given dir, and a hostile session path must not write anything
    // there (I-hostile-args). EventLog::open below replays again under
    // the held lock, so a writer landing between replay and acquire is
    // still seen.
    let events = match EventLog::replay(session_dir.join("events.jsonl")) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("overseer memory learn: {}: {e}", session_dir.display());
            return 1;
        }
    };
    let _live = match overseer_core::live::LiveLock::acquire(session_dir) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            eprintln!(
                "overseer memory learn: {} — run learn after it exits",
                overseer_core::live::BUSY
            );
            return 1;
        }
        Err(e) => {
            eprintln!("overseer memory learn: {}: {e}", session_dir.display());
            return 1;
        }
    };
    if events.is_empty() {
        eprintln!("overseer memory learn: empty event log");
        return 1;
    }
    let session_id = events
        .iter()
        .find_map(|e| match &e.kind {
            EventKind::SessionStart { session_id, .. } => Some(session_id.clone()),
            _ => None,
        })
        .unwrap_or_else(|| "unknown".into());
    // The stores come from the SESSION's recorded cwd, not the
    // invoker's — running learn from another directory must not write
    // project A's lessons into project B's store. --memory/--no-memory
    // keep their semantics, now relative to the session's project.
    let stores = session_stores(flags, &events);
    if stores.is_empty() {
        eprintln!("overseer memory learn: no memory store (see `overseer memory where`)");
        return 1;
    }
    let cursor = learn::cursor_of(&events);
    let upto = events.last().map(|e| e.id).unwrap_or(cursor);
    let stats = learn::window_stats(&events, cursor, upto);
    if stats.user_turns == 0 && stats.tool_calls == 0 {
        println!("memory learn: nothing new since e{cursor}");
        return 0;
    }
    let now = now_secs();
    let digest = learn::digest(&events, cursor, upto);
    if digest.through <= cursor {
        println!("memory learn: nothing new since e{cursor}");
        return 0;
    }
    for (scope, dir) in &stores {
        println!("{}: {}", scope.name(), dir.display());
    }
    let idx = memory::index::Index::build(&stores, now);
    let related: Vec<(String, String)> = idx
        .search(&digest.query, now)
        .iter()
        .take(8)
        .map(|h| {
            let d = &idx.docs[h.doc];
            (d.id(), memory::index::snippet(d, &digest.query, 160))
        })
        .collect();
    // DEFERRED(owner): learned-skill listing + SKILL grammar — gate: H2
    let prompt = learn::prompt(&digest, &related, &[], focus, false);

    let mut cred_cfg = agent_config(flags);
    apply_credentials(&mut cred_cfg);
    let provider: std::sync::Arc<dyn Provider> =
        match crate::provider::build_provider(flags, &cred_cfg.broker) {
            Ok(p) => p.into(),
            Err(msg) => {
                eprintln!("overseer: {msg}");
                return 2;
            }
        };
    // §1.7: ledgered and gated — every attempt lands in the session's own
    // ledger and is refused when `--max-cost` cannot cover it.
    let ledger_path = session_dir.join("ledger.jsonl");
    let ledger = if ledger_path.exists() {
        overseer_core::ledger::Ledger::open(&ledger_path)
    } else {
        overseer_core::ledger::Ledger::create(&ledger_path)
    };
    let mut ledger = match ledger {
        Ok(l) => l,
        Err(e) => {
            eprintln!("overseer memory learn: ledger: {e}");
            return 1;
        }
    };
    let models = learn::review_models(flags.small_model.as_deref(), &flags.model);
    let mut reasoners = ledger.review_reasoners.clone();
    let (reply, model, cost) = match learn::review_call(
        provider.as_ref(),
        &mut ledger,
        flags.max_cost,
        &|| 0.0,
        &models,
        &prompt,
        &mut reasoners,
    ) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("overseer memory learn: {e}");
            return 1;
        }
    };
    let parsed = learn::parse(&reply);
    let ctx = learn::ApplyCtx {
        stores: &stores,
        tainted: digest.tainted,
        attended: true,
        stage_all: flags.learn_stage,
        session_id: &session_id,
        trigger: "manual",
        through: digest.through,
        now,
    };
    let out = learn::apply(&parsed, &ctx);
    let rejected = out.rejected.len() + parsed.rejected.len();
    println!(
        "memory learn: {} applied, {} staged, {} quarantined, {} rejected",
        out.applied.len(),
        out.staged.len(),
        out.quarantined.len(),
        rejected
    );
    for line in out.applied.iter().map(|a| format!("applied {a}")) {
        println!("  {line}");
    }
    for line in out.staged.iter().map(|a| format!("staged  {a}")) {
        println!("  {line}");
    }
    for line in out.quarantined.iter().map(|a| format!("quarantined {a}")) {
        println!("  {line}");
    }
    for (l, r) in &parsed.rejected {
        eprintln!("  rejected {l}: {r}");
    }
    for r in &out.rejected {
        eprintln!("  rejected {r}");
    }
    // §1.2 trigger 5 + item-14 decision: the review event appends to the
    // session's log — EventLog::open rebuilds the chain head so the line
    // is a valid continuation, not a rewrite.
    match EventLog::open(session_dir.join("events.jsonl")) {
        Ok(mut log) => {
            if let Err(e) = log.append(EventKind::MemoryReview {
                trigger: "manual".into(),
                through: digest.through,
                applied: out.applied.clone(),
                staged: out.staged.clone(),
                quarantined: out.quarantined.clone(),
                rejected: rejected as u32,
                skipped: None,
                model,
                cost_usd: cost,
                taint: digest.taint_reason.clone(),
            }) {
                eprintln!("overseer memory learn: event append: {e}");
            } else {
                let _ = log.flush();
            }
        }
        Err(e) => eprintln!("overseer memory learn: event log: {e}"),
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `learn` resolves the project store from the SESSION's cwd, not
    /// the invoker's: running the command from another directory still
    /// lands notes in the session's own project store. (The CLI has no
    /// injectable provider, so this exercises the resolution fn itself.)
    #[test]
    fn learn_store_resolution_uses_the_session_cwd() {
        let tag = std::process::id();
        let session_cwd = std::env::temp_dir().join(format!("ov-learn-sess-{tag}"));
        let other_cwd = std::env::temp_dir().join(format!("ov-learn-other-{tag}"));
        std::fs::create_dir_all(&session_cwd).unwrap();
        std::fs::create_dir_all(&other_cwd).unwrap();
        let flags = crate::flags::parse_exec(&[
            "--cwd".into(),
            other_cwd.to_string_lossy().into_owned(),
            "x".into(),
        ])
        .unwrap();
        let events = vec![overseer_core::event::Event {
            id: 1,
            parent_id: None,
            ts_ms: 0,
            prev_hash: 0,
            hash: 0,
            kind: EventKind::SessionStart {
                session_id: "s".into(),
                cwd: session_cwd.to_string_lossy().into_owned(),
                model: "m".into(),
                harness_version: "t".into(),
                parent: None,
            },
        }];
        let stores = session_stores(&flags, &events);
        let home = overseer_core::memory::overseer_home().expect("OVERSEER_HOME/HOME set");
        let want = overseer_core::memory::stores::project_store(&home, &session_cwd);
        let wrong = overseer_core::memory::stores::project_store(&home, &other_cwd);
        assert_ne!(want, wrong, "the two cwds must map to different stores");
        assert!(
            stores
                .iter()
                .any(|(s, d)| *s == Scope::Project && *d == want),
            "expected the session's project store {want:?}, got {stores:?}"
        );
        assert!(
            !stores
                .iter()
                .any(|(s, d)| *s == Scope::Project && *d == wrong),
            "the invoker's cwd must not pick the store"
        );
    }
}
