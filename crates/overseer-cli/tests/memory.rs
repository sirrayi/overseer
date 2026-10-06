//! `overseer memory where|search` resolve stores from `$OVERSEER_HOME`
//! and the exec memory flags; every test pins a temp home.

use std::path::{Path, PathBuf};
use std::process::Command;

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ov-cli-mem-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(d.join("Repo Dir/.git")).unwrap();
    std::fs::create_dir_all(d.join("Repo Dir/sub")).unwrap();
    d
}

fn memory(home: &Path, args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .arg("memory")
        .args(args)
        .env("OVERSEER_HOME", home)
        .output()
        .expect("run overseer");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn where_prints_both_stores_keyed_by_the_git_toplevel() {
    let d = temp("where");
    let home = d.join("home");
    let sub = d.join("Repo Dir/sub");
    let (code, out) = memory(&home, &["where", "--cwd", sub.to_str().unwrap()]);
    assert_eq!(code, 0, "{out}");
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], format!("user: {}", home.join("memory").display()));
    let project = lines[1].strip_prefix("project: ").unwrap();
    let prefix = home
        .join("projects")
        .join("repo-dir-")
        .display()
        .to_string();
    assert!(
        project.starts_with(&prefix) && project.ends_with("/memory"),
        "{project}"
    );
    let (_, legacy) = memory(
        &home,
        &["where", "--memory", "--cwd", sub.to_str().unwrap()],
    );
    let canon = sub.canonicalize().unwrap();
    assert!(
        legacy.contains(&format!("project: {}", canon.join("memory").display())),
        "{legacy}"
    );
}

#[test]
fn no_memory_and_bare_turn_memory_off() {
    let d = temp("off");
    for flag in ["--no-memory", "--bare"] {
        let (code, out) = memory(
            &d.join("home"),
            &["where", flag, "--cwd", d.to_str().unwrap()],
        );
        assert_eq!((code, out.as_str()), (0, "memory: off\n"), "{flag}");
    }
}

#[test]
fn search_prints_ranked_hits() {
    let d = temp("search");
    let home = d.join("home");
    let semantic = home.join("memory/semantic");
    std::fs::create_dir_all(&semantic).unwrap();
    std::fs::write(
        semantic.join("deploy.md"),
        "# Deploy\nblue green rollout via argo\n",
    )
    .unwrap();
    std::fs::write(
        home.join("memory/INDEX.md"),
        "semantic/deploy.md — Deploy\n",
    )
    .unwrap();
    let cwd = d.to_str().unwrap();
    let (code, out) = memory(&home, &["search", "argo", "rollout", "--cwd", cwd]);
    assert_eq!(code, 0, "{out}");
    assert!(
        out.starts_with("user:semantic/deploy.md — Deploy\n  "),
        "{out}"
    );
    let (code, out) = memory(&home, &["search", "kubernetes", "--cwd", cwd]);
    assert_eq!((code, out.as_str()), (1, "no notes match `kubernetes`\n"));
    assert_eq!(memory(&home, &["search"]).0, 2, "a query is required");
}

/// Write a pending "add" record into the user store's pending/ dir.
fn stage_add(home: &Path, id: &str, slug: &str, text: &str) {
    let pend = home.join("memory/pending");
    std::fs::create_dir_all(&pend).unwrap();
    let rec = format!(
        "{{\n  \"id\": \"{id}\",\n  \"op\": \"add\",\n  \"target\": \"profile/{slug}.md\",\n  \"payload\": {{\"layer\": \"profile\", \"slug\": \"{slug}\", \"text\": \"{text}\", \"cues\": []}},\n  \"origin\": \"test\",\n  \"reason\": \"review add\",\n  \"created\": \"2026-10-06T00:00:00Z\"\n}}\n"
    );
    std::fs::write(pend.join(format!("{id}.json")), rec).unwrap();
}

#[test]
fn pending_lists_and_approve_applies_an_add() {
    let d = temp("pending");
    let home = d.join("home");
    let cwd = d.to_str().unwrap().to_string();
    let (code, out) = memory(&home, &["pending", "--cwd", &cwd]);
    assert_eq!((code, out.as_str()), (0, "memory: nothing pending\n"));

    stage_add(&home, "u-deadbeef", "editor-pref", "prefers ed over nano");
    let (code, out) = memory(&home, &["pending", "--cwd", &cwd]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("u-deadbeef"), "{out}");
    assert!(out.contains("prefers ed over nano"), "{out}");

    let (code, out) = memory(&home, &["approve", "u-deadbeef", "--cwd", &cwd]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("approved u-deadbeef"), "{out}");
    let note = home.join("memory/profile/editor-pref.md");
    assert!(std::fs::read_to_string(&note)
        .unwrap()
        .contains("prefers ed"));
    let (_, out) = memory(&home, &["pending", "--cwd", &cwd]);
    assert_eq!(out, "memory: nothing pending\n");
}

#[test]
fn reject_drops_the_record_and_unknown_ids_fail() {
    let d = temp("reject");
    let home = d.join("home");
    let cwd = d.to_str().unwrap().to_string();
    stage_add(&home, "u-cafef00d", "tea-pref", "green over black");
    let (code, _) = memory(&home, &["reject", "u-cafef00d", "--cwd", &cwd]);
    assert_eq!(code, 0);
    assert!(!home.join("memory/pending/u-cafef00d.json").exists());
    assert!(!home.join("memory/profile/tea-pref.md").exists());
    let (code, _) = memory(&home, &["approve", "u-nope0000", "--cwd", &cwd]);
    assert_eq!(code, 1, "unknown id fails");
}

#[test]
fn restore_unexpires_a_forgotten_note() {
    let d = temp("restore");
    let home = d.join("home");
    let cwd = d.to_str().unwrap().to_string();
    let dir = home.join("memory/semantic");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("old-fact.md"),
        "---\nconfidence: 0.6\nvalid_to: 2020-01-01T00:00:00Z\n---\n# Old\nan old fact\nforgotten: staged forget\n",
    )
    .unwrap();
    std::fs::write(home.join("memory/INDEX.md"), "semantic/old-fact.md — Old\n").unwrap();
    let (code, out) = memory(
        &home,
        &["restore", "user:semantic/old-fact.md", "--cwd", &cwd],
    );
    assert_eq!(code, 0, "{out}");
    let text = std::fs::read_to_string(dir.join("old-fact.md")).unwrap();
    assert!(
        !text.contains("valid_to:") && !text.contains("forgotten:"),
        "{text}"
    );
    // Live again: search finds it.
    let (code, out) = memory(&home, &["search", "old", "fact", "--cwd", &cwd]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("semantic/old-fact.md"), "{out}");
    // Restoring a live note is an error.
    let (code, _) = memory(
        &home,
        &["restore", "user:semantic/old-fact.md", "--cwd", &cwd],
    );
    assert_eq!(code, 1);
}

#[test]
fn stats_and_log_report_the_store() {
    let d = temp("stats");
    let home = d.join("home");
    let cwd = d.to_str().unwrap().to_string();
    let dir = home.join("memory");
    std::fs::create_dir_all(dir.join("semantic")).unwrap();
    std::fs::write(dir.join("semantic/deploy.md"), "# Deploy\nhow we ship\n").unwrap();
    std::fs::write(dir.join("INDEX.md"), "semantic/deploy.md — Deploy\n").unwrap();

    let (code, out) = memory(&home, &["stats", "--cwd", &cwd]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("user:"), "{out}");
    assert!(out.contains("semantic    1"), "{out}");
    assert!(out.contains("pending     0"), "{out}");
    assert!(out.contains("last review never"), "{out}");

    // No commits yet → quiet history; after a write, entries appear.
    stage_add(&home, "u-aaaabbbb", "editor-pref", "prefers ed");
    let (code, out) = memory(&home, &["approve", "u-aaaabbbb", "--cwd", &cwd]);
    assert_eq!(code, 0, "{out}");
    let (code, out) = memory(&home, &["log", "--cwd", &cwd]);
    assert_eq!(code, 0, "{out}");
    assert!(out.contains("user:"), "{out}");
    assert!(out.contains("memory: "), "{out}");
    let (code, out) = memory(&home, &["log", "--store", "project", "--cwd", &cwd]);
    assert_eq!(code, 0, "{out}");
    assert!(!out.contains("user:"), "{out}");
}

#[test]
fn learn_needs_a_real_session_and_has_early_exits() {
    let d = temp("learn");
    let home = d.join("home");
    let cwd = d.to_str().unwrap().to_string();
    // Missing session dir → error.
    let (code, _) = memory(
        &home,
        &[
            "learn",
            &d.join("nope").display().to_string(),
            "--cwd",
            &cwd,
        ],
    );
    assert_eq!(code, 1);
    // A session log with no turns or tool calls exits before any
    // provider is built (no credentials needed, no network).
    let sess = d.join("sess");
    std::fs::create_dir_all(&sess).unwrap();
    std::fs::write(
        sess.join("events.jsonl"),
        "{\"id\":1,\"parent_id\":null,\"ts_ms\":0,\"prev_hash\":0,\"hash\":0,\"type\":\"session_start\",\"session_id\":\"t\",\"cwd\":\"/tmp\",\"model\":\"m\",\"harness_version\":\"0\",\"parent\":null}\n",
    )
    .unwrap();
    let (code, out) = memory(
        &home,
        &["learn", &sess.display().to_string(), "--cwd", &cwd],
    );
    assert_eq!(
        (code, out.as_str()),
        (0, "memory learn: nothing new since e0\n")
    );
}

// F4: `memory learn` refuses a session whose live.lock another
// overseer process holds. This test re-execs itself as the holder —
// `OV_LIVE_WORKER=<session dir>\t<marker>` acquires live.lock, writes
// the marker file, then idles (a file, since libtest captures stdout).
#[test]
fn learn_refuses_a_session_open_in_another_process() {
    if let Ok(spec) = std::env::var("OV_LIVE_WORKER") {
        let (dir, marker) = spec.split_once('\t').unwrap();
        let _hold = overseer_core::live::LiveLock::acquire(std::path::Path::new(dir)).unwrap();
        std::fs::write(marker, "held").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(120));
        return;
    }
    let d = temp("live-learn");
    let sess = d.join("sess");
    std::fs::create_dir_all(&sess).unwrap();
    let marker = d.join("held.txt");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("learn_refuses_a_session_open_in_another_process")
        .env(
            "OV_LIVE_WORKER",
            format!("{}\t{}", sess.display(), marker.display()),
        )
        .spawn()
        .expect("spawn lock holder");
    // Wait for the child's flock before asking the CLI to learn.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "holder never reported ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .arg("memory")
        .arg("learn")
        .arg(&sess)
        .env("OVERSEER_HOME", d.join("home"))
        .output()
        .expect("run overseer memory learn");
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("session is open in another overseer process"),
        "{err}"
    );
}

// R5: `overseer rewind` refuses a session live in another process —
// truncating the log under a live writer would fork the hash chain.
// Same re-exec'd holder as the learn test above.
#[test]
fn rewind_refuses_a_session_open_in_another_process() {
    if let Ok(spec) = std::env::var("OV_LIVE_WORKER") {
        let (dir, marker) = spec.split_once('\t').unwrap();
        let _hold = overseer_core::live::LiveLock::acquire(std::path::Path::new(dir)).unwrap();
        std::fs::write(marker, "held").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(120));
        return;
    }
    let d = temp("live-rewind");
    let sess = d.join("sess");
    std::fs::create_dir_all(&sess).unwrap();
    let marker = d.join("held.txt");
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("rewind_refuses_a_session_open_in_another_process")
        .env(
            "OV_LIVE_WORKER",
            format!("{}\t{}", sess.display(), marker.display()),
        )
        .spawn()
        .expect("spawn lock holder");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !marker.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "holder never reported ready"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let out = Command::new(env!("CARGO_BIN_EXE_overseer"))
        .arg("rewind")
        .arg(&sess)
        .env("OVERSEER_HOME", d.join("home"))
        .output()
        .expect("run overseer rewind");
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("session is open in another overseer process"),
        "{err}"
    );
}
