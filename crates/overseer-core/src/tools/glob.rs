//! glob tool — file-name discovery, count-capped. Returns names only
//! (SWE-agent's finding: file-name lists are what the model needs from
//! discovery; more context proved counterproductive).

use serde_json::{json, Value};

use super::{need_str, resolve, schema, ToolCtx, ToolOutput};

const MAX_PATHS: usize = 500;

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "glob".into(),
        description: concat!(
            "Find files by glob pattern (e.g. '**/*.rs'). Returns paths only, ",
            "capped at 500. Skips .gitignore'd files."
        )
        .into(),
        input_schema: schema(
            json!({
                "pattern": {"type": "string", "description": "Glob pattern matched against paths relative to `path`."},
                "path": {"type": "string", "description": "Directory to search (default: working directory)."}
            }),
            &["pattern"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let pattern = match need_str(input, "pattern") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let base = input
        .get("path")
        .and_then(Value::as_str)
        .map(|p| resolve(ctx, p))
        .unwrap_or_else(|| ctx.cwd.clone());

    let glob = match globset::GlobBuilder::new(pattern).build() {
        Ok(g) => g.compile_matcher(),
        Err(e) => return ToolOutput::err(format!("Invalid glob '{pattern}': {e}")),
    };

    if !base.is_dir() {
        return ToolOutput::err(format!("{} is not a directory.", base.display()));
    }

    let mut found: Vec<String> = Vec::new();
    for entry in ignore::WalkBuilder::new(&base).hidden(true).build() {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let rel = entry.path().strip_prefix(&base).unwrap_or(entry.path());
        if glob.is_match(rel) {
            found.push(entry.path().display().to_string());
            if found.len() >= MAX_PATHS {
                break;
            }
        }
    }
    found.sort();

    if found.is_empty() {
        ToolOutput::ok(format!(
            "No files match '{pattern}' under {}.",
            base.display()
        ))
    } else {
        let mut text = found.join("\n");
        if found.len() >= MAX_PATHS {
            text.push_str(&format!(
                "\n[cap reached: {MAX_PATHS} shown — narrow the pattern]"
            ));
        }
        ToolOutput::ok(text)
    }
}
