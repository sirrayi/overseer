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
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            return ToolOutput::err(format!("Cannot create {}: {e}", parent.display()));
        }
    }
    super::snapshot(ctx, &path);
    match std::fs::write(&path, content) {
        Ok(()) => {
            reg.mark_read(&path); // writer knows the contents — editing is grounded
            ToolOutput::ok(format!(
                "Wrote {} ({} bytes).",
                path.display(),
                content.len()
            ))
        }
        Err(e) => ToolOutput::err(format!("Cannot write {}: {e}", path.display())),
    }
}
