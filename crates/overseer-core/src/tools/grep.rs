//! grep tool — ranked, count-capped content search. Respects .gitignore via
//! the `ignore` walker. SWE-agent's lesson holds: matches designed for the
//! model — file:line:content, capped, no noise.

use serde_json::{json, Value};

use super::{need_str, resolve, schema, ToolCtx, ToolOutput};

const MAX_MATCHES: usize = 200;
const MAX_LINE: usize = 500;

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "grep".into(),
        description: concat!(
            "Search file contents for a regex pattern. Returns file:line:match, ",
            "capped at 200 matches. Use `glob` when you only need file names. ",
            "Skips .gitignore'd files and binary files."
        )
        .into(),
        input_schema: schema(
            json!({
                "pattern": {"type": "string", "description": "Regex pattern to search for."},
                "path": {"type": "string", "description": "File or directory to search (default: working directory)."},
                "glob": {"type": "string", "description": "Optional glob filter on file names, e.g. '*.rs'."},
                "ignore_case": {"type": "boolean", "description": "Case-insensitive match (default false)."}
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
    let ignore_case = input
        .get("ignore_case")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let base = input
        .get("path")
        .and_then(Value::as_str)
        .map(|p| resolve(ctx, p))
        .unwrap_or_else(|| ctx.cwd.clone());

    let re = match regex_lite::RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            return ToolOutput::err(format!(
                "Invalid regex '{pattern}': {e}. Use a simpler pattern or escape metacharacters."
            ))
        }
    };

    let glob_filter = input
        .get("glob")
        .and_then(Value::as_str)
        .and_then(|g| globset::GlobBuilder::new(g).build().ok())
        .map(|g| g.compile_matcher());

    let mut matches: Vec<String> = Vec::new();
    let mut files_seen = 0usize;

    // B1-4: ripgrep fast path — `rg --json` when on PATH (10-50x, correct
    // ignore/binary semantics free). Same capped `file:line:content` token
    // shape; rg absent → embedded scanner below. Never surfaces raw JSON.
    if let Some(lines) = rg_json(
        &base,
        pattern,
        ignore_case,
        input.get("glob").and_then(|g| g.as_str()),
    ) {
        let mut out: Vec<String> = Vec::new();
        for l in lines {
            if out.len() >= MAX_MATCHES {
                break;
            }
            out.push(l);
        }
        if out.is_empty() {
            return ToolOutput::ok(format!("No matches for '{pattern}'."));
        }
        let mut text = out.join("\n");
        if out.len() >= MAX_MATCHES {
            text.push_str(&format!(
                "\n[match cap reached: {MAX_MATCHES} shown — narrow with `glob` or `path`]"
            ));
        }
        return ToolOutput::ok(text);
    }

    let mut search_file = |path: &std::path::Path, matches: &mut Vec<String>| {
        if let Some(gf) = &glob_filter {
            if let Some(name) = path.file_name() {
                if !gf.is_match(name) {
                    return;
                }
            }
        }
        let Ok(content) = std::fs::read_to_string(path) else {
            return; // binary/unreadable files skipped
        };
        files_seen += 1;
        for (i, line) in content.lines().enumerate() {
            if re.is_match(line) {
                // FAIL-1: byte-slicing panics on multibyte lines (byte 500
                // inside a char boundary). Char-safe truncation instead.
                matches.push(format!("{}:{}:{}", path.display(), i + 1, cap_line(line)));
                if matches.len() >= MAX_MATCHES {
                    return;
                }
            }
        }
    };

    if base.is_file() {
        search_file(&base, &mut matches);
    } else if base.is_dir() {
        for entry in ignore::WalkBuilder::new(&base).hidden(true).build() {
            if matches.len() >= MAX_MATCHES {
                break;
            }
            let Ok(entry) = entry else { continue };
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                search_file(entry.path(), &mut matches);
            }
        }
    } else {
        return ToolOutput::err(format!(
            "{} does not exist or is not searchable.",
            base.display()
        ));
    }

    if matches.is_empty() {
        ToolOutput::ok(format!(
            "No matches for '{pattern}' (searched {files_seen} files)."
        ))
    } else {
        let mut text = matches.join("\n");
        if matches.len() >= MAX_MATCHES {
            text.push_str(&format!(
                "\n[match cap reached: {MAX_MATCHES} shown — narrow with `glob` or `path`]"
            ));
        }
        ToolOutput::ok(text)
    }
}

/// Try `rg --json -n` for the query. `None` = rg absent/unusable → caller
/// falls back to the embedded scanner. Caps and truncation are applied by
/// the caller; this only parses match events into `path:line:content`.
fn rg_json(
    base: &std::path::Path,
    pattern: &str,
    ignore_case: bool,
    glob: Option<&str>,
) -> Option<Vec<String>> {
    let rg = which_rg()?;
    let mut cmd = std::process::Command::new(rg);
    cmd.arg("--json")
        .arg("-n")
        .arg("--max-count")
        .arg(MAX_MATCHES.to_string());
    if ignore_case {
        cmd.arg("-i");
    }
    if let Some(g) = glob {
        cmd.arg("-g").arg(g);
    }
    cmd.arg("-e").arg(pattern).arg(base);
    let out = cmd.output().ok()?;
    match out.status.code() {
        // rg exit 1 = clean no-match (summary-only stdout, zero match
        // events) → valid empty result, parsed below.
        // rg exit 2 = hard error (bad regex, unreadable path): return None
        // so the caller falls back to the embedded scanner, which reports
        // the honest `does not exist` / `Invalid regex` error. Swallowing
        // exit 2 as "no matches" would lie about hard failures.
        Some(2..) | None => return None,
        _ => {}
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = Vec::new();
    for line in text.lines() {
        let Ok(ev) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if ev.get("type").and_then(|t| t.as_str()) != Some("match") {
            continue;
        }
        let path = ev
            .pointer("/data/path/text")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let no = ev
            .pointer("/data/line_number")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let content = cap_line(
            ev.pointer("/data/lines/text")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        lines.push(format!("{path}:{no}:{content}"));
        if lines.len() >= MAX_MATCHES {
            break;
        }
    }
    Some(lines)
}

thread_local! {
    /// Thread-scoped PATH override (test seam). The B1-4 fallback test needs
    /// a PATH without `rg`; replacing `PATH` *process-wide* also hides `git`,
    /// `sh`, and `rustfmt` from every other test running concurrently, which
    /// surfaces as intermittent spawn failures elsewhere in the suite. A
    /// thread-local override keeps the scrub on the calling thread, so the
    /// fallback path is still covered without breaking test isolation.
    static PATH_OVERRIDE: std::cell::RefCell<Option<std::ffi::OsString>> =
        const { std::cell::RefCell::new(None) };
}

/// Resolve `rg` on PATH (respects the caller's PATH, so tests can scrub it
/// to force the fallback). No caching — PATH can change per call.
fn which_rg() -> Option<std::path::PathBuf> {
    let override_var = PATH_OVERRIDE.with(|p| p.borrow().clone());
    let path_var = override_var.or_else(|| std::env::var_os("PATH"))?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for name in ["rg", "rg.exe"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

/// A matched line as shown: at most [`MAX_LINE`] bytes (cut on a char
/// boundary), trailing whitespace trimmed. Shared by the rg fast path and
/// the embedded scanner so both render a hit identically.
fn cap_line(line: &str) -> &str {
    let mut end = line.len().min(MAX_LINE);
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[..end].trim_end()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{ToolCtx, ToolRegistry};

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-grep-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A PATH with no `rg` on it, scoped to this thread and restored on
    /// drop (panic-safe: a failing test cannot leak the scrub to the next
    /// test scheduled on the same worker thread).
    fn scrub_path() -> PathScrub {
        PATH_OVERRIDE
            .with(|p| *p.borrow_mut() = Some(std::ffi::OsString::from("/nonexistent-no-rg-here")));
        PathScrub
    }

    struct PathScrub;

    impl Drop for PathScrub {
        fn drop(&mut self) {
            PATH_OVERRIDE.with(|p| *p.borrow_mut() = None);
        }
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

    fn seed(dir: &std::path::Path) {
        std::fs::write(dir.join("a.rs"), "fn needle() {}\n// nothing\n").unwrap();
        std::fs::write(dir.join("b.txt"), "hay\nneedle here\n").unwrap();
    }

    #[test]
    fn rg_path_and_fallback_agree_on_shape() {
        // With rg on PATH the fast path runs; results keep file:line:content.
        let dir = tmpdir();
        seed(&dir);
        let mut c = ctx(&dir);
        let out = run(&serde_json::json!({"pattern": "needle"}), &mut c);
        assert!(!out.is_error);
        assert!(out.text.contains("needle"), "got: {}", out.text);
        assert!(out.text.contains(':'));
    }

    #[test]
    fn fallback_without_rg_on_path() {
        // Scrubbed PATH forces the embedded scanner — same contract.
        let dir = tmpdir();
        seed(&dir);
        let mut c = ctx(&dir);
        let guard = scrub_path();
        let out = run(&serde_json::json!({"pattern": "needle"}), &mut c);
        drop(guard);
        assert!(!out.is_error);
        assert!(out.text.contains("needle"), "got: {}", out.text);
    }

    #[test]
    fn rg_hard_error_falls_back_to_honest_error() {
        // D1: rg exit 2 (bad regex) → embedded fallback error, not "No matches".
        // (Runs only when rg is installed; otherwise the fallback runs anyway.)
        let dir = tmpdir();
        seed(&dir);
        let mut c = ctx(&dir);
        let out = run(&serde_json::json!({"pattern": "["}), &mut c);
        assert!(out.is_error, "bad regex must error, got: {}", out.text);
        assert!(
            out.text.contains("Invalid regex"),
            "honest regex error, got: {}",
            out.text
        );
    }

    #[test]
    fn rg_path_and_fallback_truncate_long_lines_identically() {
        let dir = tmpdir();
        std::fs::write(
            dir.join("long.txt"),
            format!("needle {}\nneedle {}\n", "x".repeat(2_000), "é".repeat(700)),
        )
        .unwrap();
        let mut c = ctx(&dir);
        let fast = run(&serde_json::json!({"pattern": "needle"}), &mut c);
        let guard = scrub_path();
        let slow = run(&serde_json::json!({"pattern": "needle"}), &mut c);
        drop(guard);
        assert!(!fast.is_error && !slow.is_error);
        assert_eq!(fast.text, slow.text, "both paths share one line cap");
        for line in slow.text.lines() {
            let content = line.splitn(3, ':').nth(2).unwrap();
            assert!(content.len() <= MAX_LINE, "{} bytes", content.len());
        }
    }

    #[test]
    fn multibyte_long_line_truncates_without_panic() {
        // FAIL-1: byte 500 inside a multibyte char must not panic.
        let dir = tmpdir();
        std::fs::write(dir.join("uni.txt"), format!("needle {}\n", "é".repeat(600))).unwrap();
        let mut c = ctx(&dir);
        let guard = scrub_path();
        let out = run(&serde_json::json!({"pattern": "needle"}), &mut c);
        drop(guard);
        assert!(!out.is_error, "must not panic/error: {}", out.text);
        assert!(out.text.contains("needle"));
    }

    #[test]
    fn respects_match_cap() {
        let dir = tmpdir();
        let big: String = (0..500).map(|i| format!("hit {i}\n")).collect();
        std::fs::write(dir.join("big.txt"), big).unwrap();
        let mut c = ctx(&dir);
        let _ = ToolRegistry::core(crate::perm::Policy::allow_all());
        let out = run(&serde_json::json!({"pattern": "hit"}), &mut c);
        assert!(out.text.contains("match cap reached"));
    }
}
