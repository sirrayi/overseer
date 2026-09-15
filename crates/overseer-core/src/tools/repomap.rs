//! `repo_map` + `symbol` tools (P3.7): the structural index is on-demand
//! (a tool call, not a resident prompt section — a rebuilt map every turn
//! would sit above the cache boundary and kill prefix stability).

use serde_json::{json, Value};

use super::{need_str, schema, ToolCtx, ToolOutput};
use crate::provider::ToolSpec;

pub fn spec_map() -> ToolSpec {
    ToolSpec {
        name: "repo_map".into(),
        description: concat!(
            "Ranked map of the repo's top symbols (~1K tokens): name, ",
            "definition site, and cross-file reference count. Use before ",
            "reading files to orient in an unfamiliar codebase."
        )
        .into(),
        input_schema: schema(json!({}), &[]),
    }
}

pub fn spec_symbol() -> ToolSpec {
    ToolSpec {
        name: "symbol".into(),
        description: concat!(
            "go_to_definition + find_references for a symbol name: every ",
            "definition site (path:line) and the files that reference it."
        )
        .into(),
        input_schema: schema(
            json!({
                "name": {
                    "type": "string",
                    "description": "Symbol name to look up (exact match)."
                }
            }),
            &["name"],
        ),
    }
}

pub fn run_map(_input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    ToolOutput::ok(crate::repomap::render_map(&ctx.cwd))
}

pub fn run_symbol(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let name = match need_str(input, "name") {
        Ok(n) => n,
        Err(e) => return e,
    };
    ToolOutput::ok(crate::repomap::lookup(&ctx.cwd, name))
}
