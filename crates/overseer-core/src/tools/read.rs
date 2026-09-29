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

    let start = offset - 1;
    let Window { lines, total } = match scan(&path, start, limit) {
        Ok(w) => w,
        Err(e) => {
            // P8-B (fzf lookup-miss hints): a missing path is usually a
            // typo — rank the sibling names so the repair is one call away
            // instead of a `glob` round trip.
            let hint = sibling_hint(&path)
                .map(|h| format!(" {h}"))
                .unwrap_or_default();
            return ToolOutput::err(format!(
                "Cannot read {}: {e}. Check the path with `glob` or `bash ls`.{hint}",
                path.display()
            ));
        }
    };

    if start >= total && total > 0 {
        return ToolOutput::err(format!(
            "offset {offset} is past end of file ({} has {total} lines).",
            path.display()
        ));
    }
    let end = start.saturating_add(limit).min(total);

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
    for (i, line) in lines.iter().enumerate() {
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

/// The requested line window plus the file's total line count.
struct Window {
    lines: Vec<String>,
    total: usize,
}

/// Stream `path` line by line, keeping only lines `start..start+limit`.
/// Memory is bounded by the window; the rest is only counted (the footer
/// needs the total). Line splitting matches `str::lines` (`\n` or `\r\n`)
/// and invalid UTF-8 anywhere fails like `read_to_string` does.
fn scan(path: &std::path::Path, start: usize, limit: usize) -> std::io::Result<Window> {
    use std::io::BufRead;
    let mut reader = std::io::BufReader::new(std::fs::File::open(path)?);
    let stop = start.saturating_add(limit);
    let mut buf = Vec::new();
    let mut lines = Vec::new();
    let mut total = 0usize;
    loop {
        buf.clear();
        if reader.read_until(b'\n', &mut buf)? == 0 {
            break;
        }
        let text = std::str::from_utf8(&buf).map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "stream did not contain valid UTF-8",
            )
        })?;
        if (start..stop).contains(&total) {
            lines.push(text.lines().next().unwrap_or("").to_string());
        }
        total += 1;
    }
    Ok(Window { lines, total })
}

/// Bounded sibling-name hint for a failed read: at most `SIBLING_SCAN`
/// directory entries are scanned (a directory with 100K files must not turn
/// one typo into a stall), and only real subsequence matches are offered.
const SIBLING_SCAN: usize = 64;

fn sibling_hint(path: &std::path::Path) -> Option<String> {
    let dir = path.parent()?;
    let want = path.file_name()?.to_string_lossy().to_string();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut names: Vec<String> = entries
        .flatten()
        .take(SIBLING_SCAN)
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    crate::fuzzy::miss_hint(&want, &names, 3)
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
    fn missing_path_hints_the_nearest_sibling() {
        // P8-B accept (fzf miss hints): a typo'd path gets a ranked hint,
        // and an unrelated name gets none (no noise).
        let dir = tmpdir();
        std::fs::write(dir.join("main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join("main.txt"), "x\n").unwrap();
        std::fs::write(dir.join("zzz.py"), "x\n").unwrap();
        let mut reg = crate::tools::ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);

        // `man.rs` is a subsequence of `main.rs` (a dropped letter — the
        // typo shape a subsequence matcher can actually repair).
        let miss = run(&serde_json::json!({"path": "man.rs"}), &mut c, &mut reg);
        assert!(miss.is_error);
        assert!(miss.text.contains("did you mean:"), "{}", miss.text);
        assert!(miss.text.contains("main.rs"), "{}", miss.text);

        let unrelated = run(&serde_json::json!({"path": "qqqq"}), &mut c, &mut reg);
        assert!(unrelated.is_error);
        assert!(
            !unrelated.text.contains("did you mean:"),
            "a non-subsequence must not hint: {}",
            unrelated.text
        );
        // The hint is bounded and never turns into an error of its own.
        assert!(sibling_hint(std::path::Path::new("/")).is_none());
    }

    /// The whole-file slicing the streaming reader replaced, kept as the
    /// byte-for-byte oracle for the rendered window and footer.
    fn oracle(content: &str, offset: usize, limit: usize) -> String {
        let lines: Vec<&str> = content.lines().collect();
        let total = lines.len();
        let start = offset - 1;
        let end = (start + limit).min(total);
        let mut out = String::new();
        for (i, line) in lines[start..end].iter().enumerate() {
            let n = start + i + 1;
            if line.len() > MAX_LINE {
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
        out
    }

    #[test]
    fn large_file_windows_are_byte_identical_to_whole_file_slicing() {
        let dir = tmpdir();
        let mut content = String::new();
        for i in 0..200_000 {
            match i % 7 {
                0 => content.push_str("\r\n"),
                1 => content.push_str(&format!("crlf {i}\r\n")),
                2 => content.push_str(&format!("{}é{i}\n", "x".repeat(2_100))),
                _ => content.push_str(&format!("line {i}\n")),
            }
        }
        content.push_str("no trailing newline\r");
        std::fs::write(dir.join("big.txt"), &content).unwrap();
        for (offset, limit) in [(1, 2_000), (99_990, 25), (199_995, 2_000), (200_001, 5)] {
            let mut reg = crate::tools::ToolRegistry::core(crate::perm::Policy::allow_all());
            let mut c = ctx(&dir);
            let out = run(
                &serde_json::json!({"path": "big.txt", "offset": offset, "limit": limit}),
                &mut c,
                &mut reg,
            );
            assert!(!out.is_error, "{}", out.text);
            assert_eq!(
                out.text,
                oracle(&content, offset, limit),
                "{offset}/{limit}"
            );
        }
        let mut reg = crate::tools::ToolRegistry::core(crate::perm::Policy::allow_all());
        let past = run(
            &serde_json::json!({"path": "big.txt", "offset": 200_002}),
            &mut ctx(&dir),
            &mut reg,
        );
        assert!(past.is_error);
        assert!(past.text.contains("has 200001 lines"), "{}", past.text);
        std::fs::write(dir.join("empty.txt"), "").unwrap();
        let empty = run(
            &serde_json::json!({"path": "empty.txt"}),
            &mut ctx(&dir),
            &mut reg,
        );
        assert_eq!(empty.text, "(empty file)");
        std::fs::write(dir.join("bad.txt"), b"ok\n\xff\n").unwrap();
        let bad = run(
            &serde_json::json!({"path": "bad.txt"}),
            &mut ctx(&dir),
            &mut reg,
        );
        assert!(bad.is_error);
        assert!(bad.text.contains("valid UTF-8"), "{}", bad.text);
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
