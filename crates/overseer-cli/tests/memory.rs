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
