//! Red team class I: `overseer memory …` with hostile arguments.
//! Uses `$OVERSEER_BIN`, else `target/release/overseer` when present,
//! else the test-profile binary. Invariants: no panic, exit 1 or 2 with
//! a one-line error, nothing written outside `$OVERSEER_HOME`.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> PathBuf {
    if let Some(b) = std::env::var_os("OVERSEER_BIN") {
        return b.into();
    }
    let rel = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/release/overseer");
    if rel.is_file() {
        return rel;
    }
    env!("CARGO_BIN_EXE_overseer").into()
}

struct Fx {
    root: PathBuf,
    home: PathBuf,
    repo: PathBuf,
}

fn fx(tag: &str) -> Fx {
    let root = std::env::temp_dir().join(format!("ov-rt-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let repo = root.join("repo");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(root.join("outside")).unwrap();
    std::fs::write(root.join("outside/secret.md"), "outside").unwrap();
    Fx { root, home, repo }
}

/// Everything under `root` except `home` (path -> bytes), to prove no
/// writes escape `$OVERSEER_HOME`.
fn outside_snapshot(f: &Fx) -> BTreeMap<PathBuf, Vec<u8>> {
    fn walk(d: &Path, skip: &Path, out: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let p = e.path();
            if p == skip {
                continue;
            }
            if p.is_dir() {
                out.insert(p.clone(), Vec::new());
                walk(&p, skip, out);
            } else {
                out.insert(p.clone(), std::fs::read(&p).unwrap_or_default());
            }
        }
    }
    let mut m = BTreeMap::new();
    walk(&f.root, &f.home, &mut m);
    m
}

struct Out {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

fn run(f: &Fx, args: &[OsString]) -> Out {
    let o = Command::new(bin())
        .arg("memory")
        .args(args)
        .arg("--cwd")
        .arg(&f.repo)
        .current_dir(&f.repo)
        .env("OVERSEER_HOME", &f.home)
        .env_remove("OPENCODE_API_KEY")
        .env_remove("ANTHROPIC_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .unwrap();
    Out {
        code: o.status.code(),
        stdout: String::from_utf8_lossy(&o.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&o.stderr).into_owned(),
    }
}

fn os(v: &[&str]) -> Vec<OsString> {
    v.iter().map(OsString::from).collect()
}

/// The error-path invariant: exit 1|2, no panic, one-line stderr (the
/// multi-line USAGE text is allowed only for exit 2 usage errors).
fn assert_clean_error(args: &[OsString], o: &Out) {
    if args.first().is_some_and(|a| a == "search")
        && o.code == Some(1)
        && o.stdout.starts_with("no notes match")
    {
        return;
    }
    assert!(
        !o.stderr.contains("panicked"),
        "{args:?} panicked: {}",
        o.stderr
    );
    assert!(
        matches!(o.code, Some(1 | 2)),
        "{args:?} exit {:?}: {}{}",
        o.code,
        o.stdout,
        o.stderr
    );
    let usage = o.stderr.starts_with("overseer memory: expected one of");
    assert!(
        usage || o.stderr.trim().lines().count() == 1,
        "{args:?} stderr not one line: {:?}",
        o.stderr
    );
}

fn hostile_cases() -> Vec<Vec<OsString>> {
    // Linux caps one argv string at 128 KiB (MAX_ARG_STRLEN).
    let huge = "a".repeat(120_000);
    let mut v: Vec<Vec<OsString>> = [
        &["approve", "../../outside/secret"][..],
        &["approve", "p-../../x"],
        &["approve", "project:proposals/../../../outside/secret.md"],
        &["approve", "/etc/passwd"],
        &["reject", "u-../../../../tmp/x"],
        &["reject", "project:proposals/../INDEX.md"],
        &["restore", "../outside/secret.md"],
        &["restore", "project:../../outside/secret.md"],
        &["restore", "project:semantic/../../x.md"],
        &["restore", "nope"],
        &["restore", "project:semantic/missing.md"],
        &["log", "--store", "../../outside"],
        &["learn", "/nonexistent/session"],
        &["learn", "../outside"],
        &["approve"],
        &["frobnicate"],
    ]
    .iter()
    .map(|a| os(a))
    .collect();
    v.push(vec!["approve".into(), huge.clone().into()]);
    v.push(vec![
        "restore".into(),
        format!("project:semantic/{huge}.md").into(),
    ]);
    v.push(vec!["learn".into(), huge.into()]);
    v
}

#[test]
fn i_hostile_args_fail_cleanly_and_write_nothing_outside() {
    let f = fx("hostile");
    let before = outside_snapshot(&f);
    for args in hostile_cases() {
        let o = run(&f, &args);
        assert_clean_error(&args, &o);
    }
    assert_eq!(before, outside_snapshot(&f));
}

#[test]
fn i_benign_commands_on_missing_store_do_not_panic() {
    let f = fx("missing");
    let before = outside_snapshot(&f);
    for args in [
        &["where"][..],
        &["pending"],
        &["stats"],
        &["log"],
        &["search", "anything"],
        &["approve", "all"],
        &["reject", "all"],
    ] {
        let a = os(args);
        let o = run(&f, &a);
        assert!(!o.stderr.contains("panicked"), "{a:?}: {}", o.stderr);
        assert!(matches!(o.code, Some(0..=2)), "{a:?} exit {:?}", o.code);
        if o.code != Some(0) {
            assert_clean_error(&a, &o);
        }
    }
    assert_eq!(before, outside_snapshot(&f));
}

#[test]
#[ignore = "redteam: I-non-utf8-argv"]
fn i_non_utf8_argv_fails_cleanly() {
    let f = fx("nonutf8");
    let mut failures = Vec::new();
    for sub in ["approve", "reject", "restore", "learn", "search"] {
        let args = vec![
            OsString::from(sub),
            OsString::from_vec(vec![b'p', b'-', 0xff, 0xfe]),
        ];
        let o = run(&f, &args);
        if o.stderr.contains("panicked") || !matches!(o.code, Some(1 | 2)) {
            failures.push(format!(
                "{sub}: exit {:?} {}",
                o.code,
                o.stderr.lines().next().unwrap_or("")
            ));
        }
    }
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn i_unset_home_fails_cleanly() {
    let f = fx("nohome");
    let o = Command::new(bin())
        .args(["memory", "where", "--cwd"])
        .arg(&f.repo)
        .env_remove("OVERSEER_HOME")
        .env_remove("HOME")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&o.stderr);
    assert!(!stderr.contains("panicked"), "{stderr}");
    assert!(o.status.code().is_some(), "killed by signal");
}

fn where_paths(f: &Fx) -> (PathBuf, PathBuf) {
    let o = run(f, &os(&["where"]));
    assert_eq!(o.code, Some(0), "{}", o.stderr);
    let mut l = o.stdout.lines();
    let user = l.next().unwrap().strip_prefix("user: ").unwrap().into();
    let project = l.next().unwrap().strip_prefix("project: ").unwrap().into();
    (user, project)
}

/// A store dir the user cannot write: reads work, writes fail with a
/// clean error, nothing panics.
#[test]
fn i_read_only_store_fails_cleanly() {
    if unsafe_is_root() {
        eprintln!("skipped: running as root");
        return;
    }
    let f = fx("ro");
    let (_, project) = where_paths(&f);
    std::fs::create_dir_all(project.join("semantic")).unwrap();
    std::fs::write(
        project.join("semantic/old.md"),
        "---\nconfidence: 0.5\nvalid_to: 2020-01-01T00:00:00Z\n---\n# Old\nold\n",
    )
    .unwrap();
    std::fs::write(project.join("INDEX.md"), "semantic/old.md — old\n").unwrap();
    for d in [project.join("semantic"), project.clone()] {
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o555)).unwrap();
    }
    std::fs::set_permissions(
        project.join("semantic/old.md"),
        std::fs::Permissions::from_mode(0o444),
    )
    .unwrap();
    let before = outside_snapshot(&f);
    for args in [
        &["pending"][..],
        &["stats"],
        &["log"],
        &["search", "old"],
        &["restore", "project:semantic/old.md"],
        &["approve", "p-abc"],
        &["reject", "all"],
    ] {
        let a = os(args);
        let o = run(&f, &a);
        eprintln!(
            "redteam I ro: {a:?} -> {:?} {}",
            o.code,
            o.stderr.lines().next().unwrap_or("")
        );
        assert!(!o.stderr.contains("panicked"), "{a:?}: {}", o.stderr);
        if o.code != Some(0) {
            assert_clean_error(&a, &o);
        }
    }
    for d in [project.clone(), project.join("semantic")] {
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    assert_eq!(before, outside_snapshot(&f));
}

fn unsafe_is_root() -> bool {
    Command::new("id")
        .arg("-u")
        .output()
        .map(|o| o.stdout.starts_with(b"0\n"))
        .unwrap_or(false)
}

/// A world-writable (0777) store: report whether the CLI tightens it
/// (memory notes are private by design).
#[test]
#[ignore = "redteam: I-world-writable-store"]
fn i_world_writable_store_is_tightened_or_refused() {
    let f = fx("w777");
    let (_, project) = where_paths(&f);
    std::fs::create_dir_all(&project).unwrap();
    std::fs::set_permissions(&project, std::fs::Permissions::from_mode(0o777)).unwrap();
    let mut seen = Vec::new();
    for args in [
        &["pending"][..],
        &["stats"],
        &["approve", "all"],
        &["reject", "all"],
        &["search", "x"],
    ] {
        let a = os(args);
        let o = run(&f, &a);
        assert!(!o.stderr.contains("panicked"), "{a:?}: {}", o.stderr);
        seen.push(format!("{a:?} -> {:?}", o.code));
    }
    let mode = std::fs::metadata(&project).unwrap().permissions().mode() & 0o777;
    eprintln!("redteam I 0777: {seen:?}; mode after {mode:o}");
    assert_eq!(mode & 0o077, 0, "store left at {mode:o}");
}

/// `approve all` never applies proposals; `reject all` drops queued ops.
#[test]
fn i_approve_all_skips_proposals_and_reject_all_clears() {
    let f = fx("all");
    let (_, project) = where_paths(&f);
    std::fs::create_dir_all(project.join("proposals")).unwrap();
    std::fs::create_dir_all(project.join("pending")).unwrap();
    std::fs::write(
        project.join("proposals/p.md"),
        "---\nprovenance: x\n---\n# P\nproposal\n",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("semantic")).unwrap();
    std::fs::write(project.join("semantic/a.md"), "# A\na\n").unwrap();
    std::fs::write(
        project.join("pending/p-a1.json"),
        r#"{"id":"p-a1","op":"forget","target":"semantic/a.md","target_sha256":null,"payload":{"reason":"x"}}"#,
    )
    .unwrap();
    let o = run(&f, &os(&["approve", "all"]));
    eprintln!(
        "redteam I approve all: {:?} {}{}",
        o.code, o.stdout, o.stderr
    );
    assert!(
        project.join("proposals/p.md").exists(),
        "approve all applied a proposal"
    );
    assert!(!project.join("semantic/p.md").exists());
    std::fs::write(
        project.join("pending/p-a2.json"),
        r#"{"id":"p-a2","op":"forget","target":"semantic/a.md","target_sha256":null,"payload":{"reason":"x"}}"#,
    )
    .unwrap();
    let o = run(&f, &os(&["reject", "all"]));
    eprintln!(
        "redteam I reject all: {:?} {}{}",
        o.code, o.stdout, o.stderr
    );
    assert!(!project.join("pending/p-a2.json").exists());
}
