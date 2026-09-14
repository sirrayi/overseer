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
        return ToolOutput::err(format!(
            "old_string not found in {}. Re-read the file around the target lines; \
             whitespace and indentation must match verbatim.",
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
    if let Err(e) = std::fs::write(&path, &replaced) {
        return ToolOutput::err(format!("Cannot write {}: {e}", path.display()));
    }

    // Report the applied hunk's location — never echo the whole file.
    let upto = replaced.find(new).unwrap_or(0);
    let line_no = replaced[..upto].matches('\n').count() + 1;
    let new_lines = new.matches('\n').count() + 1;
    ToolOutput::ok(format!(
        "Edited {}: {} replacement{} at line {} ({} line{} written).",
        path.display(),
        count,
        if count == 1 { "" } else { "s" },
        line_no,
        new_lines,
        if new_lines == 1 { "" } else { "s" }
    ))
}
