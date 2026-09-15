//! `skill` tool (P3.5): load a SKILL.md body on demand — progressive
//! disclosure for capabilities. Metadata is resident in the prompt;
//! bodies arrive provenance-wrapped.

use serde_json::{json, Value};

use super::{need_str, schema, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "skill".into(),
        description: concat!(
            "Load a skill's full instructions. The prompt's Skills section ",
            "lists available names; call this when one's description matches ",
            "the task. Returns the skill body marked with its source."
        )
        .into(),
        input_schema: schema(
            json!({
                "name": {
                    "type": "string",
                    "description": "Skill name as listed in the Skills section."
                }
            }),
            &["name"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let name = match need_str(input, "name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    match crate::skills::load(&ctx.cwd, name) {
        Ok(body) => ToolOutput::ok(body),
        Err(e) => ToolOutput::err(e),
    }
}
