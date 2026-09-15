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

    // Apply-time validation: refuse to write content we know is broken.
    // (.json is checked with the parser already in the tree; other formats
    // pass through — no invented validation.)
    if path.extension().and_then(|e| e.to_str()) == Some("json") {
        if let Err(e) = serde_json::from_str::<Value>(&replaced) {
            return ToolOutput::err(format!(
                "Edit rejected: it would produce invalid JSON in {} ({e}). \
                 The file was not modified — check the replacement syntax.",
                path.display()
            ));
        }
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
