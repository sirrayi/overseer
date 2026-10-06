//! Audit (memory v2): store resolution (`memory::stores`) — project keys,
//! git toplevel detection without spawning, and store permissions.

use std::path::{Path, PathBuf};
use std::process::Command;

use overseer_core::memory::{self, stores};

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "ov-audit-m2s-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d.canonicalize().unwrap()
}

fn git(dir: &Path, args: &[&str]) {
    let st = Command::new("git")
        .args([
            "-c",
            "user.name=a",
            "-c",
            "user.email=a@b",
            "-c",
            "init.defaultBranch=main",
        ])
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git");
    assert!(
        st.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&st.stderr)
    );
}

fn repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q"]);
    std::fs::write(dir.join("README"), "x").unwrap();
    git(dir, &["add", "."]);
    git(dir, &["commit", "-qm", "init"]);
}

#[test]
fn held_same_slug_repos_get_distinct_stores_and_subdirs_key_to_the_toplevel() {
    let root = tmp("slug");
    let home = root.join("home");
    let a = root.join("a/app");
    let b = root.join("b/app");
    repo(&a);
    repo(&b);
    std::fs::create_dir_all(a.join("src/deep")).unwrap();
    assert_ne!(
        stores::project_store(&home, &a),
        stores::project_store(&home, &b)
    );
    assert_eq!(stores::project_key(&a.join("src/deep")), a);
    assert_eq!(
        stores::project_store(&home, &a.join("src/deep")),
        stores::project_store(&home, &a)
    );
    // Non-repo dirs key to themselves.
    let plain = root.join("plain/x");
    std::fs::create_dir_all(&plain).unwrap();
    assert_eq!(stores::project_key(&plain), plain);
    // A submodule-style `.git` file is a toplevel too (git agrees).
    let sub = a.join("vendor/lib");
    std::fs::create_dir_all(&sub).unwrap();
    std::fs::write(sub.join(".git"), "gitdir: ../../.git/modules/lib\n").unwrap();
    assert_eq!(stores::project_key(&sub.join("src")), sub);
}

#[cfg(unix)]
#[test]
fn held_a_symlinked_checkout_resolves_to_the_same_store() {
    let root = tmp("symlink");
    let home = root.join("home");
    let real = root.join("real/app");
    repo(&real);
    let link = root.join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert_eq!(
        stores::project_store(&home, &link),
        stores::project_store(&home, &real)
    );
    assert_eq!(
        stores::project_store(&home, &link.join(".")),
        stores::project_store(&home, &real)
    );
}

#[cfg(unix)]
#[test]
fn held_store_dirs_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let root = tmp("perm");
    let home = root.join("home");
    let cwd = root.join("w");
    repo(&cwd);
    let dir = stores::project_store(&home, &cwd);
    memory::ensure(&dir).unwrap();
    let user = stores::user_store(&home);
    memory::ensure(&user).unwrap();
    for d in [&dir, &user] {
        for sub in [
            "",
            "semantic",
            "episodic",
            "procedural",
            "prospective",
            "profile",
        ] {
            let p = d.join(sub);
            if p.is_dir() {
                let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
                assert_eq!(mode, 0o700, "{}", p.display());
            }
        }
    }
}

/// A `git worktree` of the same repository gets a different project
/// store from its main checkout: its `.git` is a file, so it keys to
/// itself — project memory splits per worktree.
#[test]
fn a_worktree_shares_the_main_checkouts_store() {
    let root = tmp("worktree");
    let home = root.join("home");
    let main = root.join("app");
    repo(&main);
    let wt = root.join("app-feature");
    git(
        &main,
        &[
            "worktree",
            "add",
            "-q",
            wt.to_str().unwrap(),
            "-b",
            "feature",
        ],
    );
    assert_eq!(
        stores::project_store(&home, &wt),
        stores::project_store(&home, &main),
        "worktree and main checkout of one repo resolve to different stores"
    );
}

/// Moving (or renaming) a checkout changes `hash8(canonical path)`: the
/// next session starts with an empty project store and the old notes are
/// orphaned with nothing pointing at them.
#[test]
#[ignore = "deferred: H3 store identity"]
fn a_moved_checkout_keeps_its_project_store() {
    let root = tmp("moved");
    let home = root.join("home");
    let before = root.join("src/app");
    repo(&before);
    let old = stores::project_store(&home, &before);
    memory::ensure(&old).unwrap();
    std::fs::write(old.join("semantic/fact.md"), "the build needs clang\n").unwrap();
    let after = root.join("projects/app");
    std::fs::create_dir_all(after.parent().unwrap()).unwrap();
    std::fs::rename(&before, &after).unwrap();
    let new = stores::project_store(&home, &after);
    assert!(
        new.join("semantic/fact.md").is_file(),
        "moved repo resolves to {} (old store {} orphaned)",
        new.display(),
        old.display()
    );
}
