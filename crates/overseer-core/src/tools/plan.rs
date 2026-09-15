//! plan tool (P1.7 plan artifact): a no-op-by-design tracking surface.
//! The model declares/updates its step list; the engine persists it to
//! `<session>/plan.md` (+ `plan.json`) so it survives resume and stays
//! user-visible outside the transcript. It never executes anything —
//! statuses are intent, not state.

use serde_json::{json, Value};

use super::{schema, ToolCtx, ToolOutput};

const MAX_ITEMS: usize = 50;

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "plan".into(),
        description: concat!(
            "Record or update the working plan for a multi-step task. ",
            "No-op by design: it tracks intent only and executes nothing. ",
            "Use it for tasks with 3+ steps; resend the full list with ",
            "updated statuses each call. Persisted to plan.md in the ",
            "session dir — user-visible and resumable."
        )
        .into(),
        input_schema: schema(
            json!({
                "items": {
                    "type": "array",
                    "description": "The whole plan, ordered. Replaced wholesale on each call.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "One step."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"],
                        "additionalProperties": false
                    }
                }
            }),
            &["items"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let Some(items) = input.get("items").and_then(Value::as_array) else {
        return ToolOutput::err(
            "Missing required array parameter 'items' — send the full plan list.",
        );
    };
    if items.is_empty() {
        return ToolOutput::err("'items' is empty — nothing to record.");
    }
    if items.len() > MAX_ITEMS {
        return ToolOutput::err(format!(
            "'items' has {} entries — keep plans under {MAX_ITEMS} steps.",
            items.len()
        ));
    }

    let mut md = String::from("# Plan\n\n");
    // pending, in_progress, completed
    let mut counts = [0usize; 3];
    for (i, it) in items.iter().enumerate() {
        let content = it
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if content.is_empty() {
            return ToolOutput::err(format!("items[{i}].content is empty."));
        }
        let status = it
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("pending");
        let (mark, slot) = match status {
            "pending" => (" ", 0),
            "in_progress" => ("~", 1),
            "completed" => ("x", 2),
            other => {
                return ToolOutput::err(format!(
                    "items[{i}].status '{other}' — use pending|in_progress|completed."
                ))
            }
        };
        counts[slot] += 1;
        md.push_str(&format!("- [{mark}] {content}\n"));
    }

    let _ = std::fs::create_dir_all(&ctx.session_dir);
    let md_path = ctx.session_dir.join("plan.md");
    if let Err(e) = std::fs::write(&md_path, &md) {
        return ToolOutput::err(format!("cannot write {}: {e}", md_path.display()));
    }
    // Machine-readable twin for resume/tooling.
    let _ = std::fs::write(
        ctx.session_dir.join("plan.json"),
        serde_json::to_string_pretty(&json!({ "items": items })).unwrap_or_default(),
    );

    ToolOutput::ok(format!(
        "Plan saved to {} — {} in progress, {} pending, {} completed. \
         Tracking only: keep it updated as work proceeds.",
        md_path.display(),
        counts[1],
        counts[0],
        counts[2]
    ))
}
