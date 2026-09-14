//! read tool — line numbers, offset/limit partial reads, per-line cap.
//! Marks the file read for the read-before-edit invariant.

use serde_json::{json, Value};

use super::{need_str, opt_u64, resolve, schema, ToolCtx, ToolOutput, ToolRegistry};

const DEFAULT_LIMIT: u64 = 2_000;
const MAX_LINE: usize = 2_000;

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "read".into(),
        description: concat!(
            "Read a file with line numbers. Use offset/limit for partial reads of ",
            "large files instead of loading them whole. You must `read` a file ",
            "before you can `edit` it."
        )
        .into(),
        input_schema: schema(
            json!({
                "path": {"type": "string", "description": "File path, absolute or relative to the working directory."},
                "offset": {"type": "integer", "description": "1-based line number to start at (default 1)."},
                "limit": {"type": "integer", "description": "Max lines to return (default 2000)."}
            }),
            &["path"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx, reg: &mut ToolRegistry) -> ToolOutput {
    let path_str = match need_str(input, "path") {
        Ok(p) => p,
        Err(e) => return e,
    };
    let path = resolve(ctx, path_str);
    let offset = opt_u64(input, "offset").unwrap_or(1).max(1) as usize;
    let limit = opt_u64(input, "limit").unwrap_or(DEFAULT_LIMIT) as usize;

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            return ToolOutput::err(format!(
                "Cannot read {}: {e}. Check the path with `glob` or `bash ls`.",
                path.display()
            ))
        }
    };
    reg.mark_read(&path);

    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let start = offset - 1;
    if start >= total && total > 0 {
        return ToolOutput::err(format!(
            "offset {offset} is past end of file ({} has {total} lines).",
            path.display()
        ));
    }
    let end = (start + limit).min(total);

    let mut out = String::new();
    for (i, line) in lines[start..end].iter().enumerate() {
        let n = start + i + 1;
        if line.len() > MAX_LINE {
            out.push_str(&format!("{n:>6}\t{} [line truncated]\n", &line[..MAX_LINE]));
        } else {
            out.push_str(&format!("{n:>6}\t{line}\n"));
        }
    }
    if end < total {
        out.push_str(&format!(
            "[showing lines {}-{} of {}; use offset={} to continue]\n",
            start + 1,
            end,
            total,
            end + 1
        ));
    }
    if out.is_empty() {
        out.push_str("(empty file)");
    }
    ToolOutput::ok(out)
}
