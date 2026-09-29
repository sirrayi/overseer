//! write tool — whole-file create/overwrite. The right fallback when anchors
//! can't be made unique or for new files (playbook Ch.4 §2.3 takeaway 2).

use serde_json::{json, Value};

use super::{need_str, resolve, schema, ToolCtx, ToolOutput, ToolRegistry};

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "write".into(),
        description: concat!(
            "Write a whole file (create or overwrite). Prefer `edit` for existing ",
            "files — anchored edits cost fewer tokens and produce reviewable diffs. ",
            "The tool result confirms size; it does not echo the file back."
        )
        .into(),
        input_schema: schema(
            json!({
                "path": {"type": "string", "description": "File path to write."},
                "content": {"type": "string", "description": "Complete file content."}
            }),
            &["path", "content"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx, reg: &mut ToolRegistry) -> ToolOutput {
    let path_str = match need_str(input, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let content = match need_str(input, "content") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let path = resolve(ctx, path_str);
    // P8-B mode edit_globs (roo pattern): a mode may bound the file tools
    // to a path set — enforced in the tool so it cannot be bypassed.
    if !reg.edit_allowed(path_str) {
        let globs = reg
            .mode
            .map(|m| m.edit_globs.join(", "))
            .unwrap_or_default();
        return ToolOutput::err(format!(
            "Refusing to write {}: the active mode allows only {globs}.",
            path.display()
        ));
    }
    // Read-before-overwrite (Invariant 5's spirit): replacing an existing
    // file needs grounding, like `edit`. Checked before any directory or
    // checkpoint side effect, so a refused call leaves nothing behind.
    if std::fs::symlink_metadata(&path).is_ok() && !reg.was_read(&path) {
        return ToolOutput::err(format!(
            "Refusing to overwrite {}: read it first (or use edit).",
            path.display()
        ));
    }
    let target = match super::contained_target(&path, &reg.policy.root) {
        Ok(t) => t,
        Err(e) => return ToolOutput::err(e),
    };
    super::snapshot(ctx, &target);
    match super::write_no_follow(&target, content.as_bytes()) {
        Ok(()) => {
            reg.mark_read(&target); // writer knows the contents — editing is grounded
            ToolOutput::ok(format!(
                "Wrote {} ({} bytes).",
                path.display(),
                content.len()
            ))
        }
        Err(e) => ToolOutput::err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Checkpoint;
    use std::path::{Path, PathBuf};

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-write-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d.canonicalize().unwrap()
    }

    fn ctx<'a>(dir: &Path, cp: Option<&'a mut Checkpoint>) -> ToolCtx<'a> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: cp,
            sandbox: false,
            broker: None,
        }
    }

    fn reg(root: &Path) -> ToolRegistry {
        ToolRegistry::core(crate::perm::Policy::headless(root.to_path_buf()))
    }

    #[test]
    fn overwriting_an_unread_existing_file_is_refused_and_creates_nothing() {
        let dir = tmpdir("unread");
        let file = dir.join("a.txt");
        std::fs::write(&file, "original\n").unwrap();
        let mut cp = Checkpoint {
            dir: dir.join("cp"),
            done: Default::default(),
        };
        let mut r = reg(&dir);
        let mut c = ctx(&dir, Some(&mut cp));
        let out = run(
            &json!({"path": "a.txt", "content": "blind\n"}),
            &mut c,
            &mut r,
        );
        assert!(out.is_error, "{}", out.text);
        assert_eq!(
            out.text,
            format!(
                "Refusing to overwrite {}: read it first (or use edit).",
                file.display()
            )
        );
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original\n");
        assert!(
            !dir.join("cp").exists(),
            "a refused call leaves no checkpoint"
        );
        assert!(cp.done.is_empty());
    }

    #[test]
    fn overwrite_after_read_and_new_files_are_free() {
        let dir = tmpdir("free");
        let mut r = reg(&dir);
        let mut c = ctx(&dir, None);
        // New file (and new parent dirs) needs no read.
        let out = run(
            &json!({"path": "new/deep/b.txt", "content": "fresh\n"}),
            &mut c,
            &mut r,
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(dir.join("new/deep/b.txt")).unwrap(),
            "fresh\n"
        );
        // The writer knows the contents, so a second write is grounded.
        let out = run(
            &json!({"path": "new/deep/b.txt", "content": "again\n"}),
            &mut c,
            &mut r,
        );
        assert!(!out.is_error, "{}", out.text);
        // An existing file that was read may be overwritten.
        std::fs::write(dir.join("c.txt"), "old\n").unwrap();
        r.mark_read(&dir.join("c.txt"));
        let out = run(
            &json!({"path": "c.txt", "content": "new\n"}),
            &mut c,
            &mut r,
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(std::fs::read_to_string(dir.join("c.txt")).unwrap(), "new\n");
    }

    #[test]
    fn a_new_file_write_snapshots_its_canonical_path() {
        let dir = tmpdir("snap");
        let mut cp = Checkpoint {
            dir: dir.join("cp"),
            done: Default::default(),
        };
        let mut r = reg(&dir);
        {
            let mut c = ctx(&dir, Some(&mut cp));
            let out = run(
                &json!({"path": "./x/y.txt", "content": "1"}),
                &mut c,
                &mut r,
            );
            assert!(!out.is_error, "{}", out.text);
        }
        assert!(cp.done.contains(&dir.join("x/y.txt")), "{:?}", cp.done);
    }

    #[cfg(unix)]
    #[test]
    fn a_final_component_symlink_to_an_outside_file_is_refused() {
        let dir = tmpdir("ws-link");
        let outside = tmpdir("outside-file");
        let target = outside.join("victim.txt");
        std::fs::write(&target, "untouched\n").unwrap();
        std::os::unix::fs::symlink(&target, dir.join("link.txt")).unwrap();
        let mut r = reg(&dir);
        // Grounded (read), so only the containment re-check can refuse.
        r.mark_read(&dir.join("link.txt"));
        let mut c = ctx(&dir, None);
        let out = run(
            &json!({"path": "link.txt", "content": "pwned\n"}),
            &mut c,
            &mut r,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text.contains("outside the working directory"),
            "{}",
            out.text
        );
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "untouched\n");
    }

    #[cfg(unix)]
    #[test]
    fn a_parent_dir_symlink_to_outside_is_refused_and_creates_nothing() {
        let dir = tmpdir("ws-parent");
        let outside = tmpdir("outside-dir");
        std::os::unix::fs::symlink(&outside, dir.join("sub")).unwrap();
        let mut r = reg(&dir);
        let mut c = ctx(&dir, None);
        let out = run(
            &json!({"path": "sub/new.txt", "content": "x"}),
            &mut c,
            &mut r,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text.contains("outside the working directory"),
            "{}",
            out.text
        );
        assert!(!outside.join("new.txt").exists());
        let out = run(
            &json!({"path": "sub/deeper/new.txt", "content": "x"}),
            &mut c,
            &mut r,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(!outside.join("deeper").exists(), "no dirs created outside");
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_inside_the_workspace_still_writes_its_target() {
        let dir = tmpdir("ws-inner");
        std::fs::write(dir.join("real.txt"), "old\n").unwrap();
        std::os::unix::fs::symlink(dir.join("real.txt"), dir.join("alias.txt")).unwrap();
        let mut r = reg(&dir);
        r.mark_read(&dir.join("alias.txt"));
        let mut c = ctx(&dir, None);
        let out = run(
            &json!({"path": "alias.txt", "content": "new\n"}),
            &mut c,
            &mut r,
        );
        assert!(!out.is_error, "{}", out.text);
        assert_eq!(
            std::fs::read_to_string(dir.join("real.txt")).unwrap(),
            "new\n"
        );
        assert!(
            dir.join("alias.txt").is_symlink(),
            "the link itself survives"
        );
    }

    #[cfg(unix)]
    #[test]
    fn open_no_follow_refuses_a_symlink_swapped_in_after_resolution() {
        let dir = tmpdir("swap");
        let outside = tmpdir("swap-out");
        std::fs::write(outside.join("v.txt"), "keep\n").unwrap();
        std::os::unix::fs::symlink(outside.join("v.txt"), dir.join("t.txt")).unwrap();
        // The resolved target's final component is now a symlink: the
        // no-follow open must refuse rather than write through it.
        let err = crate::tools::write_no_follow(&dir.join("t.txt"), b"x").unwrap_err();
        assert!(!err.is_empty());
        assert_eq!(
            std::fs::read_to_string(outside.join("v.txt")).unwrap(),
            "keep\n"
        );
    }
}
