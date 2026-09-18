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

    // P1.3 read dedup (path + mtime + range): an unchanged re-read whose
    // range is already covered returns a stub instead of the same bytes.
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    if reg.dedup_hit(&path, mtime, start, end) {
        reg.mark_read(&path);
        return ToolOutput::ok(format!(
            "[unchanged] {} lines {}-{} already returned this session; \
             the file has not been modified since.",
            path.display(),
            start + 1,
            end
        ));
    }
    let mut out = String::new();
    if reg.mtime_changed(&path, mtime) {
        out.push_str("[file modified since your previous read]\n");
    }
    for (i, line) in lines[start..end].iter().enumerate() {
        let n = start + i + 1;
        if line.len() > MAX_LINE {
            // FAIL-2: same char-boundary class as FAIL-1 — truncate safely.
            let head: String = line.chars().take(MAX_LINE).collect();
            out.push_str(&format!("{n:>6}\t{head} [line truncated]\n"));
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
    reg.record_read(&path, mtime, start, end);
    ToolOutput::ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-read-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn ctx(dir: &std::path::Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: None,
            sandbox: false,
            broker: None,
        }
    }

    #[test]
    fn multibyte_long_line_truncates_without_panic() {
        // FAIL-2: byte 2000 inside a multibyte char must not panic.
        let dir = tmpdir();
        let line = format!("{}{}tail", "x".repeat(1999), "é");
        std::fs::write(dir.join("uni.txt"), &line).unwrap();
        let mut reg = crate::tools::ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);
        let out = run(&serde_json::json!({"path": "uni.txt"}), &mut c, &mut reg);
        assert!(!out.is_error, "must not panic: {}", out.text);
        assert!(out.text.contains("[line truncated]"));
    }
}
