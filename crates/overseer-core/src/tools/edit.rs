//! edit tool — anchored search/replace (playbook Ch.4 §2.3):
//! exact-match + uniqueness + read-before-edit, validated at apply time.
//! The result returns the applied hunk location, never the whole file.

use serde_json::{json, Value};

use super::{need_str, resolve, schema, ToolCtx, ToolOutput, ToolRegistry};

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "edit".into(),
        description: concat!(
            "Replace an exact string in a file. `old_string` must match the file ",
            "content verbatim (including indentation) and be unique unless ",
            "replace_all is set. The file must have been read with `read` or ",
            "created with `write` earlier in this session. On failure, read the ",
            "relevant lines again and retry with more surrounding context."
        )
        .into(),
        input_schema: schema(
            json!({
                "path": {"type": "string", "description": "File path to edit."},
                "old_string": {"type": "string", "description": "Exact text to find; must occur exactly once unless replace_all."},
                "new_string": {"type": "string", "description": "Replacement text."},
                "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)."}
            }),
            &["path", "old_string", "new_string"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx, reg: &mut ToolRegistry) -> ToolOutput {
    let path_str = match need_str(input, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let old = match need_str(input, "old_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let new = match need_str(input, "new_string") {
        Ok(s) => s,
        Err(e) => return e,
    };
    let replace_all = input
        .get("replace_all")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let path = resolve(ctx, path_str);

    // Read-before-edit: harness-enforced anti-hallucination invariant.
    if !reg.was_read(&path) {
        return ToolOutput::err(format!(
            "Refusing to edit {}: it has not been read this session. \
             Call `read` on it first — editing ungrounded content corrupts files.",
            path.display()
        ));
    }
    if old == new {
        return ToolOutput::err("old_string and new_string are identical — nothing to change.");
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("Cannot read {}: {e}", path.display())),
    };

    let count = content.matches(old).count();
    if count == 0 {
        let line_count = content.lines().count();
        let fallback = if line_count <= WHOLE_FILE_FALLBACK_LINES {
            format!(
                " The file is only {line_count} lines — rewriting it wholesale with \
                 `write` is an acceptable fallback."
            )
        } else {
            String::new()
        };
        return ToolOutput::err(format!(
            "old_string not found in {}. Re-read the file around the target lines; \
             whitespace and indentation must match verbatim.{fallback}",
            path.display()
        ));
    }
    if count > 1 && !replace_all {
        return ToolOutput::err(format!(
            "old_string occurs {count} times in {} — the edit must be unique. \
             Include more surrounding context, or set replace_all to change all {count}.",
            path.display()
        ));
    }

    let replaced = if replace_all {
        content.replacen(old, new, usize::MAX)
    } else {
        content.replacen(old, new, 1)
    };

    // Apply-time validation (B1-1 ACI lint gate): refuse to write content we
    // know is broken. Parse-only probes — the gate signal is a parse failure,
    // never a formatting diff:
    // - .json: parser already in the tree.
    // - .py: `python3 -m py_compile` on a temp copy (parse-only; imports OK).
    // - .rs: `rustfmt --edition 2021 --check` on a temp copy, gated on
    //   stderr matching `^error` (parse failure). `--check` exits 1 on mere
    //   formatting Diffs from valid-but-unformatted code (Diff goes to
    //   stdout, stderr stays empty), so the exit code alone must NOT gate.
    // Gate binary missing → apply (fail-open), never block. No new deps.
    if let Some(diag) = edit_lint_gate(&path, &replaced) {
        return ToolOutput::err(format!(
            "Edit rejected: it would produce invalid {} in {} ({diag}). \
             The file was not modified — check the replacement syntax.",
            path.extension().and_then(|e| e.to_str()).unwrap_or("file"),
            path.display()
        ));
    }

    super::snapshot(ctx, &path);
    if let Err(e) = std::fs::write(&path, &replaced) {
        return ToolOutput::err(format!("Cannot write {}: {e}", path.display()));
    }

    // Return the applied hunk (±3 lines context) — never the whole file.
    // The first match's offset in the original equals where `new` landed
    // in `replaced` (works for deletions too, where `new` is empty).
    let upto = content.find(old).unwrap_or(0);
    let line_no = replaced[..upto].matches('\n').count() + 1;
    let new_lines = if new.is_empty() {
        0
    } else {
        new.matches('\n').count() + 1
    };
    ToolOutput::ok(format!(
        "Edited {}: {} replacement{} at line {}.\n{}",
        path.display(),
        count,
        if count == 1 { "" } else { "s" },
        line_no,
        hunk(&replaced, line_no, new_lines)
    ))
}

/// Parse-only lint gate over post-edit content (B1-1). Returns `Some(diag)`
/// when the content is unparseable, `None` when it parses or the gate does
/// not apply (unknown extension, gate binary missing). Pure function of
/// (path, content) — no fs writes outside a temp file, no network.
pub fn edit_lint_gate(path: &std::path::Path, content: &str) -> Option<String> {
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str::<Value>(content)
            .err()
            .map(|e| e.to_string()),
        Some("py") => probe_tempfile(content, "py", &["python3", "-m", "py_compile"], false),
        Some("rs") => probe_tempfile(
            content,
            "rs",
            &["rustfmt", "--edition", "2021", "--check"],
            true,
        ),
        _ => None,
    }
}

/// Stage `content` as a temp file and run a parse probe. When
/// `stderr_error_gate` is set, only stderr lines starting with `error` block
/// (rustfmt `--check` formatting Diffs go to stdout and must NOT gate).
/// Binary missing or temp unwritable → None (fail-open). No new deps.
fn probe_tempfile(
    content: &str,
    ext: &str,
    cmd: &[&str],
    stderr_error_gate: bool,
) -> Option<String> {
    let mut tmp = std::env::temp_dir();
    tmp.push(format!("overseer-lint-{}", uuid::Uuid::now_v7()));
    tmp.set_extension(ext);
    if std::fs::write(&tmp, content).is_err() {
        return None;
    }
    let out = std::process::Command::new(cmd[0])
        .args(&cmd[1..])
        .arg(&tmp)
        .output();
    let _ = std::fs::remove_file(&tmp);
    match out {
        Err(_) => None,
        Ok(o) if !stderr_error_gate && o.status.success() => None,
        Ok(o) if !stderr_error_gate => Some(first_lines(&String::from_utf8_lossy(&o.stderr), 3)),
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            if stderr.lines().any(|l| l.starts_with("error")) {
                Some(first_lines(&stderr, 3))
            } else {
                None
            }
        }
    }
}

/// First `n` lines of a diagnostic, single-line-joined for tool output.
fn first_lines(s: &str, n: usize) -> String {
    s.lines().take(n).collect::<Vec<_>>().join(" | ")
}

/// Files at or under this many lines get the whole-file `write` fallback
/// hint when an anchor fails (playbook: ~400 lines).
const WHOLE_FILE_FALLBACK_LINES: usize = 400;

/// Render the applied hunk with line numbers: the new content plus 3 lines
/// of context each side. Long replacements are head/tail-trimmed so the
/// result stays a hunk, not a file dump.
fn hunk(content: &str, line_no: usize, new_lines: usize) -> String {
    const CONTEXT: usize = 3;
    const MAX_HUNK: usize = 60;

    let lines: Vec<&str> = content.lines().collect();
    let first = line_no.saturating_sub(CONTEXT).max(1);
    let last = (line_no + new_lines - 1 + CONTEXT).min(lines.len());
    let mut out = String::new();
    let mut shown = 0usize;
    let mut i = first;
    while i <= last {
        let in_new = i >= line_no && i < line_no + new_lines;
        // Trim the middle of large inserted regions — context rows stay.
        if in_new && new_lines > MAX_HUNK && shown >= MAX_HUNK / 2 {
            let remaining = (line_no + new_lines) - i;
            if remaining > MAX_HUNK / 2 {
                out.push_str(&format!(
                    "      [...{} inserted lines omitted...]\n",
                    remaining - MAX_HUNK / 2
                ));
                i = line_no + new_lines - MAX_HUNK / 2;
                continue;
            }
        }
        out.push_str(&format!("{i:>6}\t{}\n", lines[i - 1]));
        shown += 1;
        i += 1;
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lint_path(name: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(name)
    }

    #[test]
    fn lint_gate_blocks_invalid_json() {
        let d = edit_lint_gate(&lint_path("a.json"), r#"{"a": }"#);
        assert!(d.is_some(), "invalid JSON must block");
        assert!(edit_lint_gate(&lint_path("a.json"), r#"{"a": 1}"#).is_none());
    }

    #[test]
    fn lint_gate_blocks_invalid_python() {
        let d = edit_lint_gate(&lint_path("a.py"), "def f(:\n  pass\n");
        assert!(d.is_some(), "invalid Python must block");
    }

    #[test]
    fn lint_gate_allows_valid_python_with_imports() {
        let ok = "import os\n\n\ndef f():\n    return os.getcwd()\n";
        assert!(edit_lint_gate(&lint_path("a.py"), ok).is_none());
    }

    #[test]
    fn lint_gate_blocks_invalid_rust() {
        let d = edit_lint_gate(&lint_path("a.rs"), "fn main( { broken\n");
        assert!(d.is_some(), "unparseable Rust must block");
    }

    #[test]
    fn lint_gate_allows_valid_rust_with_imports() {
        let ok = "use std::collections::HashMap;\n\nfn main() {\nlet m = HashMap::new();\nprintln!(\"{:?}\", m.len());\n}\n";
        assert!(
            edit_lint_gate(&lint_path("a.rs"), ok).is_none(),
            "valid Rust with imports must apply"
        );
    }

    #[test]
    fn lint_gate_allows_valid_but_unformatted_rust() {
        // rustfmt --check exits 1 with a Diff on stdout for this file, but
        // stderr carries no `error` — the gate signal is stderr, not exit code.
        let ok = "fn main() { let x = 1; println!(\"{x}\"); }\n";
        assert!(
            edit_lint_gate(&lint_path("a.rs"), ok).is_none(),
            "valid-but-unformatted Rust must apply"
        );
    }

    #[test]
    fn lint_gate_ignores_unknown_extensions() {
        assert!(edit_lint_gate(&lint_path("a.md"), "{{{broken").is_none());
    }
}
