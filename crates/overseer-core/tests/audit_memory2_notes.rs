//! Audit (memory v2): the memory tool, recall notices, prospective
//! reminders and session episodes, driven through their public APIs
//! (`MemoryState`, `notice`, `episode`, `index::Index`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use overseer_core::agent::AgentConfig;
use overseer_core::event::{Event, EventKind};
use overseer_core::memory::{self, episode, index::Index, Scope};
use overseer_core::tools::memory_tool::MemoryState;
use serde_json::json;

static SEQ: AtomicUsize = AtomicUsize::new(0);

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ov-audit-m2-{tag}-{}-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Env {
    root: PathBuf,
    user: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new(tag: &str) -> Env {
        let root = tmp(tag);
        let user = root.join("home/memory");
        let project = root.join("home/projects/w-0000/memory");
        memory::ensure(&user).unwrap();
        memory::ensure(&project).unwrap();
        Env {
            root,
            user,
            project,
        }
    }

    fn config(&self, subagent: bool) -> AgentConfig {
        AgentConfig {
            cwd: self.root.clone(),
            memory_dir: Some(self.project.clone()),
            user_memory_dir: Some(self.user.clone()),
            memory_recall: true,
            is_subagent: subagent,
            ..AgentConfig::default()
        }
    }

    fn state(&self, session: &str) -> MemoryState {
        let mut st = MemoryState::default();
        st.init(&self.config(false), &self.root.join(session));
        st
    }

    fn stores(&self) -> Vec<(Scope, PathBuf)> {
        vec![
            (Scope::User, self.user.clone()),
            (Scope::Project, self.project.clone()),
        ]
    }

    fn note(&self, rel: &str, text: &str) {
        let p = self.project.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn all_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let p = e.path();
            if e.file_type().unwrap().is_dir() {
                if e.file_name() != ".git" {
                    stack.push(p);
                }
            } else {
                out.push(p);
            }
        }
    }
    out
}

fn ghp() -> String {
    format!("ghp_{}", "Zx9Kq2Lm8Np4Rs6Tu1Vw3Xy5Ab7Cd0Ef2Gh4")
}

fn ev(id: u64, kind: EventKind) -> Event {
    Event {
        id,
        parent_id: None,
        ts_ms: 1_790_000_000_000 + id,
        prev_hash: 0,
        hash: 0,
        kind,
    }
}

fn session_events(prompt: &str, runs: &[u32]) -> Vec<Event> {
    let mut kinds = vec![
        EventKind::SessionStart {
            session_id: "1790000000123".into(),
            cwd: "/w".into(),
            model: "m1".into(),
            harness_version: "0".into(),
            parent: None,
        },
        EventKind::UserInput {
            text: prompt.into(),
        },
    ];
    for &steps in runs {
        kinds.push(EventKind::RunEnd {
            stop_reason: "completed".into(),
            steps,
            total_cost_usd: 0.25,
            cache: Default::default(),
            subagent_cost_usd: 0.0,
        });
    }
    kinds
        .into_iter()
        .enumerate()
        .map(|(i, k)| ev(i as u64, k))
        .collect()
}

// =====================================================================
// Findings
// =====================================================================

/// `remember` scrubs `text` but writes `cues` into the frontmatter raw.
#[test]
#[ignore = "audit: memory-cues"]
fn remember_scrubs_secrets_in_cues() {
    let env = Env::new("cues");
    let mut st = env.state("session-0192aabbccdd");
    let key = ghp();
    let out = st.run(
        &json!({"op": "remember", "layer": "semantic", "name": "deploy",
                "text": "deploy via blue green", "cues": format!("deploy, {key}")}),
        None,
        now(),
    );
    assert!(!out.is_error, "{}", out.text);
    let note = read(&env.project.join("semantic/deploy.md"));
    assert!(!note.contains(&key), "secret persisted in cues:\n{note}");
}

/// A secret passed as `name` becomes the file name, the INDEX pointer
/// and the git commit — only lowercased by `slugify`.
#[test]
#[ignore = "audit: memory-name"]
fn remember_scrubs_secrets_in_the_note_name() {
    let env = Env::new("name");
    let mut st = env.state("session-0192aabbccdd");
    let key = ghp();
    let out = st.run(
        &json!({"op": "remember", "layer": "semantic", "name": key, "text": "the ci token"}),
        None,
        now(),
    );
    assert!(!out.is_error, "{}", out.text);
    let low = key.to_lowercase().replace('_', "-");
    let leaked: Vec<_> = all_files(&env.project)
        .into_iter()
        .filter(|p| p.to_string_lossy().contains(&low) || read(p).contains(&low))
        .collect();
    assert!(leaked.is_empty(), "token persisted via name: {leaked:?}");
}

/// `forget`'s reason is appended as `forgotten: <reason>` unscrubbed.
#[test]
#[ignore = "audit: memory-forgetreason"]
fn forget_scrubs_secrets_in_the_reason() {
    let env = Env::new("forgetwhy");
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    st.run(
        &json!({"op": "remember", "layer": "semantic", "name": "ci", "text": "ci uses a pat"}),
        None,
        t,
    );
    let key = ghp();
    let out = st.run(
        &json!({"op": "forget", "name": "project:semantic/ci.md",
                "text": format!("rotated, new one is {key}")}),
        None,
        t,
    );
    assert!(!out.is_error, "{}", out.text);
    let note = read(&env.project.join("semantic/ci.md"));
    assert!(
        !note.contains(&key),
        "secret persisted in forget reason:\n{note}"
    );
}

/// `forget` stamps `valid_to: now`, but a note is expired only when
/// `valid_to < now` — for the rest of that second it still searches,
/// recalls and serves.
#[test]
#[ignore = "audit: memory-forgetnow"]
fn a_forgotten_note_is_gone_in_the_same_second() {
    let env = Env::new("forgetnow");
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    st.run(
        &json!({"op": "remember", "layer": "semantic", "name": "old", "text": "legacy kiwi endpoint"}),
        None,
        t,
    );
    let out = st.run(
        &json!({"op": "forget", "name": "project:semantic/old.md", "text": "retired"}),
        None,
        t,
    );
    assert!(!out.is_error, "{}", out.text);
    let search = st.run(
        &json!({"op": "search", "query": "legacy kiwi endpoint"}),
        None,
        t,
    );
    assert!(
        !search.text.contains("semantic/old.md"),
        "forgotten note still searchable: {}",
        search.text
    );
    let get = st.run(
        &json!({"op": "get", "name": "project:semantic/old.md"}),
        None,
        t,
    );
    assert!(get.is_error, "forgotten note still served: {}", get.text);
}

/// `remember` to an existing name appends through `OpenOptions::append`,
/// which follows a symlink planted at the note path: a write outside the
/// store (reachable when the store sits in the workspace, `--memory`).
#[cfg(unix)]
#[test]
#[ignore = "audit: memory-symlink-append"]
fn remember_append_never_writes_through_a_symlink() {
    let env = Env::new("symappend");
    let outside = env.root.join("outside.txt");
    std::fs::write(&outside, "ORIGINAL\n").unwrap();
    std::os::unix::fs::symlink(&outside, env.project.join("semantic/x.md")).unwrap();
    let mut st = env.state("session-0192aabbccdd");
    let out = st.run(
        &json!({"op": "remember", "layer": "semantic", "name": "x", "text": "appended fact"}),
        None,
        now(),
    );
    assert_eq!(
        read(&outside),
        "ORIGINAL\n",
        "remember wrote outside the store ({})",
        out.text
    );
}

/// `episode::write` rewrites `episodic/session-<date>-<id8>.md` with
/// `fs::write`, following a symlink planted there between two run ends.
#[cfg(unix)]
#[test]
#[ignore = "audit: episode-symlink"]
fn episode_rewrite_never_writes_through_a_symlink() {
    let env = Env::new("symepisode");
    let events = session_events("fix the build", &[3]);
    let rel = episode::write(&env.project, &events).unwrap().unwrap();
    let outside = env.root.join("dotfile");
    std::fs::write(&outside, "ORIGINAL\n").unwrap();
    let ep = env.project.join(&rel);
    std::fs::remove_file(&ep).unwrap();
    std::os::unix::fs::symlink(&outside, &ep).unwrap();
    let events = session_events("fix the build", &[3, 4]);
    let _ = episode::write(&env.project, &events);
    assert_eq!(
        read(&outside),
        "ORIGINAL\n",
        "episode clobbered a file outside the store"
    );
}

/// A path reminder fires against the index as of its last refresh and
/// stamps `fired:` with `fs::write` on the doc path — through a symlink
/// swapped in after the index was built.
#[cfg(unix)]
#[test]
#[ignore = "audit: notice-symlink-fire"]
fn firing_a_reminder_never_writes_through_a_symlink() {
    let env = Env::new("symfire");
    env.note(
        "prospective/parser.md",
        "---\ntrigger: path:src/**\n---\nreview the parser invariants\n",
    );
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    st.on_input("start working on the lexer today", t);
    let outside = env.root.join("dotfile");
    std::fs::write(&outside, "ORIGINAL\n").unwrap();
    let p = env.project.join("prospective/parser.md");
    std::fs::remove_file(&p).unwrap();
    std::os::unix::fs::symlink(&outside, &p).unwrap();
    st.observe("read", &json!({"path": "src/lib.rs"}), true, &env.root, t);
    let fired = st.take_queued();
    assert_eq!(
        read(&outside),
        "ORIGINAL\n",
        "fire() wrote outside the store (notices: {})",
        fired.len()
    );
}

/// Re-running `episode::write` (every run end, and every resumed run)
/// regenerates the file from the log, dropping a `forget`'s `valid_to`:
/// the forgotten episode is current and searchable again.
#[test]
#[ignore = "audit: episode-resurrect"]
fn a_forgotten_episode_stays_forgotten_after_the_next_run_end() {
    let env = Env::new("resurrect");
    let events = session_events("migrate the kiwi billing tables", &[3]);
    let rel = episode::write(&env.project, &events).unwrap().unwrap();
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    let out = st.run(
        &json!({"op": "forget", "name": format!("project:{rel}"), "text": "wrong"}),
        None,
        t,
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(read(&env.project.join(&rel)).contains("valid_to:"));
    // Resume: one more run ends in the same session.
    episode::write(
        &env.project,
        &session_events("migrate the kiwi billing tables", &[3, 2]),
    )
    .unwrap();
    let idx = Index::build(&env.stores(), t + 5);
    let hits: Vec<String> = idx
        .search("kiwi billing tables", t + 5)
        .iter()
        .map(|h| idx.docs[h.doc].id())
        .collect();
    assert!(
        !hits.contains(&format!("project:{rel}")),
        "forgotten episode is current again: {hits:?}\n{}",
        read(&env.project.join(&rel))
    );
}

struct Capture(std::sync::Mutex<Vec<String>>);

impl overseer_core::provider::Provider for Capture {
    fn complete(
        &self,
        req: &overseer_core::provider::Request,
    ) -> Result<overseer_core::provider::Response, overseer_core::provider::ProviderError> {
        let all: Vec<String> = req.messages.iter().map(|m| m.text()).collect();
        self.0.lock().unwrap().push(all.join("\n"));
        Ok(overseer_core::provider::Response {
            blocks: vec![overseer_core::ir::Block::Text {
                text: String::new(),
            }],
            stop_reason: overseer_core::provider::StopReason::EndTurn,
            usage: overseer_core::ir::Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        })
    }
    fn name(&self) -> &'static str {
        "capture"
    }
}

/// `distill` reads every file in `episodic/` without a validity check:
/// a forgotten episode is still sent to the model and distilled.
#[test]
#[ignore = "audit: episode-distill-forgotten"]
fn distill_skips_forgotten_episodes() {
    let env = Env::new("distill");
    let events = session_events("rotate the zebra credentials on host omega", &[3]);
    let rel = episode::write(&env.project, &events).unwrap().unwrap();
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    st.run(
        &json!({"op": "forget", "name": format!("project:{rel}"), "text": "do not keep"}),
        None,
        t,
    );
    let cap = Capture(Default::default());
    let _ = episode::distill(&cap, "small", &env.stores(), t + 5);
    let seen = cap.0.lock().unwrap().join("\n");
    assert!(
        !seen.contains("zebra credentials"),
        "forgotten episode sent to the distiller:\n{seen}"
    );
}

/// Fired claims are keyed by `rel` alone and never released: a new
/// reminder at a previously used name never fires.
#[test]
#[ignore = "audit: notice-claimreuse"]
fn a_new_reminder_at_a_reused_name_fires() {
    let env = Env::new("claim");
    env.note(
        "prospective/renew.md",
        "---\ntrigger: kw:renew\n---\nrenew the tls cert\n",
    );
    let mut st = env.state("session-0192aabbccdd");
    let n = st.on_input("please renew it", now());
    assert_eq!(n.iter().filter(|n| n.kind == "reminder").count(), 1);
    // The user deletes the fired reminder and later writes a new one.
    std::fs::remove_file(env.project.join("prospective/renew.md")).unwrap();
    env.note(
        "prospective/renew.md",
        "---\ntrigger: kw:invoice\n---\nrenew the domain before billing\n",
    );
    let mut st2 = env.state("session-0192aabbcc99");
    let n = st2.on_input("the invoice arrived", now());
    assert_eq!(
        n.iter().filter(|n| n.kind == "reminder").count(),
        1,
        "the new reminder never fires"
    );
}

// =====================================================================
// Held (coverage record)
// =====================================================================

#[test]
fn held_recall_is_capped_and_filters_expired_proposals_prospective_episodes_and_other_stores() {
    let env = Env::new("recall");
    for i in 0..6 {
        env.note(
            &format!("semantic/kiwi-{i}.md"),
            &format!("# Kiwi pipeline {i}\nthe kiwi deploy pipeline runs stage {i}\n"),
        );
    }
    env.note(
        "semantic/expired.md",
        "---\nvalid_to: 2020-01-01T00:00:00Z\n---\nkiwi deploy pipeline kiwi deploy pipeline\n",
    );
    env.note(
        "semantic/superseded.md",
        "---\nconfidence: 0.9\n---\nkiwi deploy pipeline old\nsuperseded_by semantic/kiwi-0.md\n",
    );
    env.note(
        "proposals/kiwi.md",
        "kiwi deploy pipeline kiwi deploy pipeline\n",
    );
    env.note(
        "prospective/kiwi.md",
        "---\ntrigger: kw:zzzz\n---\nkiwi deploy pipeline\n",
    );
    env.note(
        "episodic/session-x.md",
        "---\nprovenance: engine\n---\nkiwi deploy pipeline kiwi deploy pipeline\n",
    );
    // Another project's store, not configured for this session.
    let other = env.root.join("home/projects/other-1111/memory");
    memory::ensure(&other).unwrap();
    std::fs::write(other.join("semantic/kiwi.md"), "kiwi deploy pipeline\n").unwrap();

    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    let first = st.on_input("kiwi deploy pipeline", t);
    let recall: Vec<_> = first.iter().filter(|n| n.kind == "recall").collect();
    assert_eq!(recall.len(), 1);
    let ids = &recall[0].notes;
    assert!(ids.len() <= 3, "{ids:?}");
    assert!(recall[0].text.len() <= 1200);
    for id in ids {
        assert!(id.starts_with("project:semantic/kiwi-"), "{id}");
    }
    let again = st.on_input("kiwi deploy pipeline", t);
    for n in again.iter().filter(|n| n.kind == "recall") {
        assert!(n.notes.iter().all(|id| !ids.contains(id)), "re-recalled");
    }
    let mut sub = MemoryState::default();
    sub.init(&env.config(true), &env.root.join("session-sub"));
    assert!(sub
        .on_input("how does the kiwi deploy pipeline work", t)
        .is_empty());
}

#[test]
fn held_kw_at_and_path_reminders_fire_once_and_never_from_tool_output() {
    let env = Env::new("remind");
    env.note(
        "prospective/rb.md",
        "---\ntrigger: kw:rollback\n---\ncheck the rollback plan\n",
    );
    env.note(
        "prospective/at.md",
        "---\ntrigger: at:2020-01-01T00:00:00Z\n---\nsend the report\n",
    );
    env.note(
        "prospective/p.md",
        "---\ntrigger: path:src/**/*.rs\n---\nrun clippy\n",
    );
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    let first = st.on_input("starting work on the docs", t);
    let kinds: Vec<_> = first.iter().map(|n| (n.kind, n.notes.clone())).collect();
    assert_eq!(
        first.iter().filter(|n| n.kind == "reminder").count(),
        1,
        "{kinds:?}"
    );
    // Tool calls and their output never fire kw: reminders.
    st.observe(
        "bash",
        &json!({"command": "git rollback", "path": "src/a.rs"}),
        true,
        &env.root,
        t,
    );
    st.observe("read", &json!({"path": "rollback.md"}), true, &env.root, t);
    st.observe("read", &json!({"path": "src/a.rs"}), false, &env.root, t);
    assert!(st.take_queued().is_empty());
    st.observe("read", &json!({"path": "src/x/a.rs"}), true, &env.root, t);
    st.observe("edit", &json!({"path": "src/x/b.rs"}), true, &env.root, t);
    assert_eq!(st.take_queued().len(), 1, "path: fires once");
    let kw = st.on_input("plan the rollback", t);
    assert_eq!(kw.iter().filter(|n| n.kind == "reminder").count(), 1);
    assert!(st
        .on_input("rollback now", t)
        .iter()
        .all(|n| n.kind != "reminder"));
    let mut st2 = env.state("session-0192aabbcc77");
    assert!(st2
        .on_input("rollback again", t)
        .iter()
        .all(|n| n.kind != "reminder"));
}

#[test]
fn held_get_never_leaves_the_store_and_skips_symlinked_notes() {
    let env = Env::new("get");
    let outside = env.root.join("secret.txt");
    std::fs::write(&outside, "TOP SECRET kiwi\n").unwrap();
    env.note("semantic/real.md", "# Real\nkiwi real note\n");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, env.project.join("semantic/link.md")).unwrap();
    env.note("proposals/prop.md", "kiwi proposal\n");
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    for name in [
        "../../secret.txt",
        "/etc/passwd",
        outside.to_str().unwrap(),
        "project:../../../secret.txt",
        "user:../projects/w-0000/memory/semantic/real.md",
        "semantic/../../../secret.txt",
        "proposals/prop.md",
        "project:semantic/link.md",
        "link",
    ] {
        let out = st.run(&json!({"op": "get", "name": name}), None, t);
        assert!(
            out.is_error && !out.text.contains("TOP SECRET"),
            "{name}: {}",
            out.text
        );
    }
    let s = st.run(&json!({"op": "search", "query": "kiwi secret"}), None, t);
    assert!(
        !s.text.contains("TOP SECRET") && !s.text.contains("link.md"),
        "{}",
        s.text
    );
    assert!(
        !st.run(&json!({"op": "get", "name": "real"}), None, t)
            .is_error
    );
}

#[test]
fn held_pathological_queries_return_quickly_without_panicking() {
    let env = Env::new("patho");
    for i in 0..300 {
        env.note(
            &format!("semantic/n{i}.md"),
            &format!("# Note {i}\nalpha beta gamma {i}\n"),
        );
    }
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    let many: String = (0..5000).map(|i| format!("w{i} ")).collect();
    let qs = vec![
        "".to_string(),
        "   ".into(),
        "the a of and to".into(),
        "x".repeat(200_000),
        "alpha ".repeat(20_000),
        many,
        "🦀🦀 ∑∂ 日本語".into(),
        "\u{0}\u{1}\u{7f}".into(),
        "(((( [[[[ ** ?? \\\\".into(),
        "İSTANBUL ǅ ß ﬁ".into(),
        "a-b_c.d/e::f<g>".into(),
        "\u{200b}\u{202e}alpha".into(),
    ];
    for q in qs {
        let start = std::time::Instant::now();
        let _ = st.run(&json!({"op": "search", "query": q}), None, t);
        let _ = st.on_input(&q, t);
        assert!(
            start.elapsed().as_secs_f64() < 3.0,
            "slow query len {}",
            q.len()
        );
    }
}

#[test]
fn held_forget_expires_without_deleting_and_write_cap_holds() {
    let env = Env::new("cap");
    let mut st = env.state("session-0192aabbccdd");
    let t = now();
    for i in 0..5 {
        let out = st.run(
            &json!({"op": "remember", "layer": "semantic", "name": format!("n{i}"), "text": format!("fact number {i}")}),
            None,
            t,
        );
        assert!(!out.is_error, "{}", out.text);
    }
    let over = st.run(
        &json!({"op": "forget", "name": "project:semantic/n0.md", "text": "x"}),
        None,
        t,
    );
    assert!(over.is_error, "6th write in one turn must be refused");
    st.on_input("next turn please", t);
    let out = st.run(
        &json!({"op": "forget", "name": "project:semantic/n0.md", "text": "x"}),
        None,
        t,
    );
    assert!(!out.is_error, "{}", out.text);
    let p = env.project.join("semantic/n0.md");
    assert!(p.is_file(), "forget never deletes");
    assert!(read(&p).contains("fact number 0"));
    assert!(
        st.run(
            &json!({"op": "get", "name": "project:semantic/n0.md"}),
            None,
            t + 2
        )
        .is_error
    );
}

#[test]
fn held_episode_bounds_redaction_and_rewrite_idempotence() {
    let env = Env::new("episode");
    let long = format!("deploy with {} {}", ghp(), "word ".repeat(2000));
    let events = session_events(&long, &[3]);
    let ep = episode::derive(&events).unwrap();
    assert!(!ep.text.contains(&ghp()));
    // "Session <date> <id8>: " + the 60-char prompt line.
    assert!(ep.title.chars().count() <= 29 + 61, "{}", ep.title);
    assert!(ep.text.len() < 2_000, "{}", ep.text.len());
    assert!(
        episode::derive(&session_events("x", &[1])).is_none(),
        "below MIN_STEPS"
    );
    let rel = episode::write(&env.project, &events).unwrap().unwrap();
    let a = read(&env.project.join(&rel));
    assert_eq!(episode::write(&env.project, &events).unwrap().unwrap(), rel);
    assert_eq!(read(&env.project.join(&rel)), a, "same log, same bytes");
    episode::write(&env.project, &session_events(&long, &[3, 4])).unwrap();
    let index = read(&env.project.join("INDEX.md"));
    assert_eq!(index.matches(&rel).count(), 1);
}
