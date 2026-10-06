//! Audit (test/audit-surfaces): CLI argv parsing, exit codes, session
//! resolution, `--bare` isolation, key hygiene. Every test drives the
//! real `overseer` binary with a scrubbed environment and a temp HOME.
//! Tests marked `#[ignore = "audit: ..."]` reproduce a defect on b9eae9b.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_overseer");
const SENTINEL: &str = "sk-audit-SENTINEL-0123456789abcdef";

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "audit-cli-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn cmd(home: &Path) -> Command {
    // Private TMPDIR per test: `--bare` dirs are keyed by unix-ms (see
    // `concurrent_bare_runs_never_collide`), so parallel tests sharing
    // /tmp would flake.
    let t = home.join("tmp");
    std::fs::create_dir_all(&t).unwrap();
    let mut c = Command::new(BIN);
    c.env_clear()
        .env("TMPDIR", t)
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", home)
        .env("OPENAI_API_KEY", SENTINEL)
        .stdin(Stdio::null());
    c
}

/// Provider flags that fail fast with a transport error (exit 4).
const DEAD: &[&str] = &[
    "--provider",
    "openai",
    "--base-url",
    "http://127.0.0.1:1/v1",
    "--model",
    "gpt-audit",
];

fn run(home: &Path, args: &[&str]) -> Output {
    cmd(home).args(args).output().unwrap()
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

fn no_panic(o: &Output) {
    let t = text(o);
    assert!(!t.contains("panicked"), "panic: {t}");
    assert_ne!(o.status.code(), Some(101), "{t}");
}

// ── argv parsing / exit codes ──────────────────────────────────────────

#[test]
#[ignore = "audit: cli-non-utf8-argv — other wave"]
fn non_utf8_argv_is_a_usage_error_not_a_panic() {
    use std::os::unix::ffi::OsStrExt;
    let home = tmp("utf8");
    for args in [
        vec![
            std::ffi::OsStr::from_bytes(b"exec"),
            std::ffi::OsStr::from_bytes(b"\xff"),
        ],
        vec![
            std::ffi::OsStr::from_bytes(b"exec"),
            std::ffi::OsStr::from_bytes(b"--cwd"),
            std::ffi::OsStr::from_bytes(b"/tmp/caf\xe9"),
            std::ffi::OsStr::from_bytes(b"hi"),
        ],
    ] {
        let o = cmd(&home).args(args).output().unwrap();
        no_panic(&o);
        assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    }
}

#[test]
fn every_subcommand_refuses_unknown_flags_with_exit_2() {
    let home = tmp("unknown");
    for sub in [
        "exec",
        "tui",
        "web",
        "stats",
        "rewind",
        "mcp",
        "memory",
        "daemon",
        "inbox",
        "trigger",
        "channel",
        "consolidate",
        "onboard",
    ] {
        let o = run(&home, &[sub, "--definitely-not-a-flag"]);
        no_panic(&o);
        assert_eq!(o.status.code(), Some(2), "{sub}: {}", text(&o));
    }
    let o = run(&home, &["--definitely-not-a-flag"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
    let o = run(&home, &["no-such-subcommand"]);
    assert_eq!(o.status.code(), Some(2), "{}", text(&o));
}

#[test]
fn missing_values_and_malformed_flags_exit_2() {
    let home = tmp("values");
    let cases: &[&[&str]] = &[
        &["exec", "--model"],
        &["exec", "hi", "--max-steps"],
        &["exec", "hi", "--max-steps", "abc"],
        &["exec", "hi", "--max-steps=-1"],
        &["exec", "hi", "--max-cost", "lots"],
        &["exec", "hi", "--provider", "bogus"],
        &["exec", "hi", "--provider="],
        &["exec", "hi", "--json=1"],
        &["exec", "hi", "--effort", "extreme"],
        &["exec", "--bare", "--continue", "hi"],
        &["exec", "--bare", "--last", "hi"],
        &["exec", "--bare", "--resume", "/nope", "hi"],
        &["exec", "--bare", "--session", "/nope", "hi"],
        &["exec", "--api-key", "x", "hi"],
        &["tui", "--port", "1"],
        &["web", "--port", "notaport"],
        &["web", "--port"],
        &["stats"],
        &["rewind"],
        &["rewind", "/x", "--mode", "nuke"],
        &["rewind", "/x", "--checkpoint", "abc"],
        &["mcp"],
        &["mcp", "frob"],
        &["memory"],
        &["memory", "approve"],
        &["daemon", "frob"],
        &["inbox", "frob"],
        &["inbox", "approve"],
        &["trigger"],
        &["trigger", "fire", "--source", "x"],
        &["channel"],
        &["channel", "send", "--to", "x"],
        &["consolidate", "--bare"],
        &["tui", "--no-tui", "some prompt"],
    ];
    for args in cases {
        let o = run(&home, args);
        no_panic(&o);
        assert_eq!(o.status.code(), Some(2), "{args:?}: {}", text(&o));
    }
}

/// `--max-cost NaN` parses as f64 NaN; every `spent > max` comparison is
/// then false, so the cost budget (invariant 3) silently never trips.
#[test]
#[ignore = "audit: cli-max-cost-nan — other wave"]
fn max_cost_nan_is_rejected() {
    let home = tmp("nan");
    let mut a = vec!["exec", "--bare", "--max-cost", "NaN"];
    a.extend_from_slice(DEAD);
    a.push("hi");
    let o = run(&home, &a);
    assert_eq!(
        o.status.code(),
        Some(2),
        "NaN budget accepted: {}",
        text(&o)
    );
}

/// `-h` after `--` is a literal prompt, not a help request.
#[test]
#[ignore = "audit: cli-dashdash-help — other wave"]
fn help_flag_after_double_dash_is_a_positional() {
    let home = tmp("dashdash");
    let mut a = vec!["exec", "--bare"];
    a.extend_from_slice(DEAD);
    a.extend_from_slice(&["--", "-h"]);
    let o = run(&home, &a);
    // Reaching the (dead) provider = exit 4; printing usage = exit 0.
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
}

// ── key hygiene ────────────────────────────────────────────────────────

/// One-shot fake OpenAI endpoint: answers every request with `status`
/// and `body`, records each raw request.
fn fake_openai(status: &'static str, body: String) -> (String, std::sync::mpsc::Receiver<String>) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/v1", l.local_addr().unwrap());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { return };
            let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut head = String::new();
            let mut cl = 0usize;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    cl = v.trim().parse().unwrap_or(0);
                }
                head.push_str(&line);
                if line == "\r\n" {
                    break;
                }
            }
            let mut b = vec![0u8; cl];
            let _ = r.read_exact(&mut b);
            let _ = tx.send(head + &String::from_utf8_lossy(&b));
            let _ = write!(
                s,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    (url, rx)
}

#[test]
fn provider_key_never_appears_in_output_or_errors() {
    let home = tmp("keys");
    let mut a = vec!["exec", "--bare"];
    a.extend_from_slice(DEAD);
    a.push("hi");
    let o = run(&home, &a);
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
    assert!(!text(&o).contains(SENTINEL));

    let (url, _rx) = fake_openai(
        "401 Unauthorized",
        r#"{"error":{"message":"Incorrect API key provided","type":"invalid_request_error"}}"#
            .into(),
    );
    let o = run(
        &home,
        &[
            "exec",
            "--bare",
            "--provider",
            "openai",
            "--base-url",
            &url,
            "--model",
            "m",
            "hi",
        ],
    );
    assert_ne!(o.status.code(), Some(0), "{}", text(&o));
    assert!(!text(&o).contains(SENTINEL), "key leaked: {}", text(&o));
}

// ── session resolution ─────────────────────────────────────────────────

fn events(dir: &Path) -> usize {
    std::fs::read_to_string(dir.join("events.jsonl"))
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn exec_in(home: &Path, ws: &Path, extra: &[&str]) -> Output {
    let mut a = vec!["exec", "--no-memory", "--cwd", ws.to_str().unwrap()];
    a.extend_from_slice(DEAD);
    a.extend_from_slice(extra);
    a.push("hi");
    run(home, &a)
}

#[test]
fn resume_beats_session_and_continue() {
    let home = tmp("prec-home");
    let ws = tmp("prec-ws");
    let s1 = home.join("s1");
    let s2 = home.join("s2");
    let o = exec_in(&home, &ws, &["--session", s1.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
    let before = events(&s1);
    assert!(before > 0);
    let o = exec_in(
        &home,
        &ws,
        &[
            "--resume",
            s1.to_str().unwrap(),
            "--session",
            s2.to_str().unwrap(),
            "--continue",
        ],
    );
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
    assert!(events(&s1) > before, "resume did not append to s1");
    assert!(!s2.exists(), "--session created a dir despite --resume");
}

#[test]
fn continue_beats_session_and_scopes_to_cwd() {
    let home = tmp("cont-home");
    let ws = tmp("cont-ws");
    let other = tmp("cont-other");
    let o = exec_in(&home, &ws, &[]);
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
    let root = home.join(".overseer").join("sessions");
    let first: Vec<_> = std::fs::read_dir(&root).unwrap().flatten().collect();
    assert_eq!(first.len(), 1);
    let sdir = first[0].path();
    let before = events(&sdir);
    // A later session in a different cwd must not be picked by --continue.
    std::thread::sleep(Duration::from_millis(20));
    let o = exec_in(&home, &other, &[]);
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
    let s3 = home.join("s3");
    let o = exec_in(
        &home,
        &ws,
        &["--continue", "--session", s3.to_str().unwrap()],
    );
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
    assert!(
        events(&sdir) > before,
        "--continue did not resume the cwd's session"
    );
    assert!(!s3.exists());
}

#[test]
fn bare_exec_never_touches_home_overseer() {
    let home = tmp("bare-home");
    let ws = tmp("bare-ws");
    let mut a = vec!["exec", "--bare", "--cwd", ws.to_str().unwrap()];
    a.extend_from_slice(DEAD);
    a.push("hi");
    let o = run(&home, &a);
    assert_eq!(o.status.code(), Some(4), "{}", text(&o));
    assert!(!home.join(".overseer").exists());
    assert!(!ws.join(".overseer").exists());
}

/// `--bare` sessions live at `$TMPDIR/overseer-bare-<unix-ms>`: two runs
/// started in the same millisecond collide (the second dies with
/// "cannot start session: File exists", exit 1). Parallel CI/eval
/// workers hit this.
#[test]
#[ignore = "audit: cli-bare-dir — other wave"]
fn concurrent_bare_runs_never_collide() {
    let home = tmp("bare-par");
    let tmpdir = tmp("bare-par-tmp");
    let mut a = vec!["exec", "--bare"];
    a.extend_from_slice(DEAD);
    a.push("hi");
    let kids: Vec<_> = (0..12)
        .map(|_| {
            cmd(&home)
                .env("TMPDIR", &tmpdir)
                .args(&a)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap()
        })
        .collect();
    let codes: Vec<_> = kids
        .into_iter()
        .map(|k| {
            let o = k.wait_with_output().unwrap();
            (
                o.status.code(),
                String::from_utf8_lossy(&o.stderr).into_owned(),
            )
        })
        .collect();
    for (c, e) in &codes {
        assert_eq!(*c, Some(4), "bare run failed: {e}");
    }
}

/// The predictable bare path is adopted even when the directory already
/// exists (create_dir_all), so on a shared /tmp another user can
/// pre-create it and own the directory the session writes into.
#[test]
#[ignore = "audit: cli-bare-dir — other wave"]
fn bare_run_never_adopts_a_preexisting_directory() {
    let home = tmp("bare-plant");
    let tmpdir = tmp("bare-plant-tmp");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    for ms in now..now + 4000 {
        std::fs::create_dir(tmpdir.join(format!("overseer-bare-{ms}"))).unwrap();
    }
    let mut a = vec!["exec", "--bare"];
    a.extend_from_slice(DEAD);
    a.push("hi");
    let o = cmd(&home).env("TMPDIR", &tmpdir).args(&a).output().unwrap();
    let adopted: Vec<_> = std::fs::read_dir(&tmpdir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().join("events.jsonl").exists())
        .map(|e| e.file_name())
        .collect();
    let _ = std::fs::remove_dir_all(&tmpdir);
    assert!(
        adopted.is_empty(),
        "session written into planted dir {adopted:?}: {}",
        text(&o)
    );
}

/// `exec` rejects `--bare` with resume flags (exit 2, "hermetic"); the
/// tui/web entry points share the flag set but skip the check, so
/// `--resume` is silently dropped and a fresh throwaway session starts.
#[test]
#[ignore = "audit: cli-tui-bare-resume — other wave"]
fn tui_rejects_bare_with_resume_flags() {
    let home = tmp("tui-bare");
    for flag in [
        &["--resume", "/nonexistent"][..],
        &["--continue"],
        &["--last"],
    ] {
        let mut a = vec!["tui", "--no-tui", "--bare"];
        a.extend_from_slice(DEAD);
        a.extend_from_slice(flag);
        let mut c = cmd(&home);
        c.args(&a)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = c.spawn().unwrap();
        let _ = child.stdin.take().unwrap().write_all(b"/quit\n");
        let o = child.wait_with_output().unwrap();
        assert_eq!(o.status.code(), Some(2), "{flag:?}: {}", text(&o));
    }
}

/// Line mode prints `Cell::plain()` raw: model text carrying OSC 52 /
/// OSC 0 reaches the user's terminal.
#[test]
fn line_mode_does_not_forward_model_escape_sequences() {
    let home = tmp("line-osc");
    let evil = "hello \u{1b}]52;c;cHduZWQ=\u{7} \u{1b}]0;pwned\u{7} done";
    let body = serde_json::json!({
        "choices": [{"message": {"role": "assistant", "content": evil}, "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1}
    })
    .to_string();
    let (url, _rx) = fake_openai("200 OK", body);
    let mut c = cmd(&home);
    c.args([
        "tui",
        "--no-tui",
        "--bare",
        "--provider",
        "openai",
        "--base-url",
        &url,
        "--model",
        "m",
    ])
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    stdin.write_all(b"say hi\n").unwrap();
    std::thread::sleep(Duration::from_secs(3));
    let _ = stdin.write_all(b"/quit\n");
    drop(stdin);
    let o = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&o.stdout);
    assert!(
        out.contains("done"),
        "model reply never printed: {}",
        text(&o)
    );
    assert!(!out.contains('\u{1b}'), "raw ESC forwarded: {out:?}");
}

// ── gateway ctl clients ────────────────────────────────────────────────

/// `inbox snooze <id> [<s>]` is documented in seconds, but values
/// ≥ 10 000 are passed through as milliseconds: a one-day snooze
/// (86400) becomes 86.4 s.
#[test]
#[ignore = "audit: cli-inbox-snooze-units — other wave"]
fn inbox_snooze_seconds_are_seconds_for_large_values() {
    use std::os::unix::net::UnixListener;
    let home = tmp("snooze");
    let dd = tmp("snooze-dd");
    let mut got = Vec::new();
    for secs in ["60", "86400"] {
        let sock = dd.join("daemon.sock");
        let _ = std::fs::remove_file(&sock);
        let l = UnixListener::bind(&sock).unwrap();
        let h = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            let mut line = String::new();
            BufReader::new(s.try_clone().unwrap())
                .read_line(&mut line)
                .unwrap();
            let _ = (&s).write_all(b"{\"ok\":true}\n");
            line
        });
        let o = run(
            &home,
            &[
                "inbox",
                "snooze",
                "item1",
                secs,
                "--dir",
                dd.to_str().unwrap(),
            ],
        );
        assert_eq!(o.status.code(), Some(0), "{}", text(&o));
        let req: serde_json::Value = serde_json::from_str(&h.join().unwrap()).unwrap();
        got.push(req["snooze_ms"].as_u64());
    }
    assert_eq!(got[0], Some(60_000));
    assert_eq!(
        got[1],
        Some(86_400_000),
        "1-day snooze sent as {:?} ms",
        got[1]
    );
}
