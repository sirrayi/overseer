//! `diagnostics` tool — compiler diagnostics without the build log
//! (opencode's LSP-diagnostics pattern, arsenal B2).
//!
//! The model's loop after an edit is "run the build, read the errors".
//! A raw `cargo check` output is mostly progress noise; the diagnostics
//! tool runs the same check with `--message-format=json` and returns only
//! the *messages*, one line each, `file:line:col: level: message`.
//!
//! Fail-open is the contract: if `cargo` is missing, the workspace has no
//! manifest, the check times out, or the stream is unparseable, the tool
//! reports that plainly and does NOT error — a diagnostics probe must
//! never be the reason a turn dies. A non-zero exit is expected and
//! meaningful (errors present); it never turns into a tool error.
//!
//! `// DEFERRED(owner): LSP diagnostics for non-Rust files (@typescript-
//! language-server et al.) — cargo is the only host toolchain this port
//! shells out to; an LSP client is P8-C+ material. The check runs
//! unsandboxed (it is a workspace-local read-only probe); route it through
//! the bash sandbox when that grows a non-shell entry point.`

use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use super::{schema, ToolCtx, ToolOutput};

/// Wall-clock cap for the check; a runaway build never blocks a turn.
const CHECK_TIMEOUT_S: u64 = 120;
/// Model-facing diagnostic cap (one line each).
const MAX_DIAGS: usize = 60;
/// Cap on the raw stdout we read back before parsing.
const MAX_STDOUT: usize = 4_000_000;

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "diagnostics".into(),
        description: concat!(
            "Run `cargo check --message-format=json` and return compiler ",
            "diagnostics as `file:line:col: level: message` lines. Use after an ",
            "edit to see real errors instead of the whole build log. Fails open ",
            "(missing cargo / timeout → a note, never an error)."
        )
        .into(),
        input_schema: schema(
            json!({
                "package": {
                    "type": "string",
                    "description": "Restrict the check to one workspace package (`cargo check -p <package>`)."
                },
                "level": {
                    "type": "string",
                    "enum": ["error", "warning", "all"],
                    "description": "Which diagnostics to return (default: error)."
                }
            }),
            &[],
        ),
    }
}

/// One compiler message, flattened to what the model needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub level: String,
    pub message: String,
    pub file: Option<String>,
    pub line: Option<u64>,
    pub column: Option<u64>,
}

/// Parse `cargo --message-format=json` stdout. Unknown/malformed lines are
/// skipped (cargo interleaves non-JSON build-script output); a message
/// with no `level` is skipped too. First line of `message` only — cargo's
/// `rendered` block is the pretty form we are deliberately not sending.
pub fn parse_lines(stdout: &str) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if !line.starts_with('{') {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(msg) = v.get("message") else {
            continue;
        };
        let Some(level) = msg.get("level").and_then(Value::as_str) else {
            continue;
        };
        let text = msg
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .lines()
            .next()
            .unwrap_or_default()
            .trim()
            .to_string();
        // Primary span first, else the first span at all.
        let spans = msg.get("spans").and_then(Value::as_array);
        let span = spans.and_then(|s| {
            s.iter()
                .find(|sp| sp.get("is_primary").and_then(Value::as_bool) == Some(true))
                .or_else(|| s.first())
        });
        out.push(Diagnostic {
            level: level.to_string(),
            message: text,
            file: span
                .and_then(|s| s.get("file_name"))
                .and_then(Value::as_str)
                .map(str::to_string),
            line: span
                .and_then(|s| s.get("line_start"))
                .and_then(Value::as_u64),
            column: span
                .and_then(|s| s.get("column_start"))
                .and_then(Value::as_u64),
        });
    }
    out
}

/// Keep the requested levels, in cargo's emission order. Borrows so the
/// caller can still report the total (e.g. "N lower-severity suppressed").
pub fn filter_level(diags: &[Diagnostic], level: &str) -> Vec<Diagnostic> {
    match level {
        "all" => diags.to_vec(),
        "warning" => diags
            .iter()
            .filter(|d| d.level == "warning" || d.level == "error")
            .cloned()
            .collect(),
        _ => diags
            .iter()
            .filter(|d| d.level == "error")
            .cloned()
            .collect(),
    }
}

/// Render diagnostics as one bounded block of `file:line:col: level: msg`.
pub fn render(diags: &[Diagnostic], max: usize) -> String {
    let shown = diags.len().min(max);
    let mut out = String::new();
    for d in &diags[..shown] {
        let loc = match (&d.file, d.line, d.column) {
            (Some(f), Some(l), Some(c)) => format!("{f}:{l}:{c}"),
            (Some(f), Some(l), None) => format!("{f}:{l}"),
            (Some(f), None, _) => f.clone(),
            _ => "?".to_string(),
        };
        out.push_str(&format!("{loc}: {}: {}\n", d.level, d.message));
    }
    if diags.len() > shown {
        out.push_str(&format!(
            "[{} more diagnostics omitted — narrow with `package`]\n",
            diags.len() - shown
        ));
    }
    out.trim_end().to_string()
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let package = input.get("package").and_then(Value::as_str);
    let level = input
        .get("level")
        .and_then(Value::as_str)
        .unwrap_or("error");
    let argv = check_argv(package);
    let args: Vec<&str> = argv.iter().map(String::as_str).collect();
    collect(&ctx.cwd, "cargo", &args, level)
}

/// The `cargo check` argv for an optional package (pure — the tool's only
/// policy decision, so it is testable without spawning a toolchain).
pub fn check_argv(package: Option<&str>) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "check".into(),
        "--message-format=json".into(),
        // --quiet keeps cargo's own progress lines out of the stream we
        // parse; the diagnostics are the point, not the build log.
        "--quiet".into(),
    ];
    if let Some(p) = package {
        args.push("-p".into());
        args.push(p.to_string());
    }
    args
}

/// Run `program args…` in `cwd` and turn its JSON stream into a result.
/// Every failure mode is a note, never a tool error (fail-open).
fn collect(cwd: &Path, program: &str, args: &[&str], level: &str) -> ToolOutput {
    let log_path = std::env::temp_dir().join(format!("overseer-diag-{}.log", uuid::Uuid::now_v7()));
    let Ok(file) = std::fs::File::create(&log_path) else {
        return ToolOutput::ok(
            "diagnostics: cannot create a temp log — skipped (fail-open).".into(),
        );
    };
    let Ok(err_file) = file.try_clone() else {
        let _ = std::fs::remove_file(&log_path);
        return ToolOutput::ok(
            "diagnostics: cannot create a temp log — skipped (fail-open).".into(),
        );
    };
    let child = Command::new(program)
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(file)
        .stderr(err_file)
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_file(&log_path);
            return ToolOutput::ok(format!(
                "diagnostics: `{program}` is unavailable ({e}) — skipped (fail-open)."
            ));
        }
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(CHECK_TIMEOUT_S);
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            Ok(None) => {
                timed_out = true;
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&log_path);
                return ToolOutput::ok(format!(
                    "diagnostics: check failed to run ({e}) — skipped (fail-open)."
                ));
            }
        }
    }
    let stdout = read_capped(&log_path, MAX_STDOUT);
    let _ = std::fs::remove_file(&log_path);

    let all = parse_lines(&stdout);
    let kept = filter_level(&all, level);
    let mut text = if kept.is_empty() {
        if all.is_empty() {
            if timed_out {
                format!(
                    "diagnostics: `{program} {}` timed out after {CHECK_TIMEOUT_S}s — no \
                     diagnostics reported (fail-open).",
                    args.join(" ")
                )
            } else if stdout.trim().is_empty() {
                format!(
                    "diagnostics: `{program} {}` produced no diagnostics — clear.",
                    args.join(" ")
                )
            } else {
                "diagnostics: no compiler messages in the check output (fail-open).".to_string()
            }
        } else {
            format!(
                "diagnostics: no `{level}` diagnostics ({} lower-severity message(s) suppressed).",
                all.len()
            )
        }
    } else {
        format!(
            "diagnostics: {} message(s) at level `{level}`.\n{}",
            kept.len(),
            render(&kept, MAX_DIAGS)
        )
    };
    if timed_out && !kept.is_empty() {
        text.push_str("\n[check timed out — the list is partial]");
    }
    ToolOutput::ok(text)
}

/// Read up to `cap` bytes from a file, best-effort and char-boundary safe.
fn read_capped(path: &Path, cap: usize) -> String {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let mut buf = Vec::new();
    let _ = f.by_ref().take(cap as u64).read_to_end(&mut buf);
    String::from_utf8_lossy(&buf).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(dir: &Path) -> ToolCtx<'static> {
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

    fn tmpdir() -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-diag-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    const STREAM: &str = concat!(
        "{\"reason\":\"compiler-artifact\",\"package_id\":\"x\"}\n",
        "   Compiling nonsense\n",
        "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"warning\",\"message\":\"unused variable: `y`\",",
        "\"spans\":[{\"file_name\":\"src/lib.rs\",\"line_start\":3,\"column_start\":9,\"is_primary\":true}]}}\n",
        "{\"reason\":\"compiler-message\",\"message\":{\"level\":\"error\",\"message\":\"mismatched types\\nexpected `u8`, found `u16`\",",
        "\"spans\":[{\"file_name\":\"src/main.rs\",\"line_start\":12,\"column_start\":5,\"is_primary\":false},",
        "{\"file_name\":\"src/helper.rs\",\"line_start\":40,\"column_start\":1,\"is_primary\":true}]}}\n",
        "{\"reason\":\"build-finished\",\"success\":false}\n",
    );

    #[test]
    fn parses_compiler_messages_and_prefers_the_primary_span() {
        let diags = parse_lines(STREAM);
        assert_eq!(
            diags.len(),
            2,
            "only compiler-message lines are diagnostics"
        );
        assert_eq!(diags[0].level, "warning");
        assert_eq!(diags[0].file.as_deref(), Some("src/lib.rs"));
        assert_eq!((diags[0].line, diags[0].column), (Some(3), Some(9)));
        // Multi-line message → first line only.
        assert_eq!(diags[1].message, "mismatched types");
        assert_eq!(diags[1].file.as_deref(), Some("src/helper.rs"));
        assert_eq!(diags[1].line, Some(40));
        // Level filter: default keeps errors, `all` keeps everything.
        assert_eq!(filter_level(&diags, "error").len(), 1);
        assert_eq!(filter_level(&diags, "all").len(), 2);
        assert_eq!(filter_level(&diags, "warning").len(), 2);
    }

    #[test]
    fn render_is_one_line_per_diagnostic_and_bounded() {
        let diags = parse_lines(STREAM);
        let errs = filter_level(&diags, "error");
        let text = render(&errs, MAX_DIAGS);
        assert_eq!(text, "src/helper.rs:40:1: error: mismatched types");
        // No span at all → the location is `?`, the message still lands.
        let bare = render(
            &[Diagnostic {
                level: "error".into(),
                message: "aborting due to 2 previous errors".into(),
                file: None,
                line: None,
                column: None,
            }],
            MAX_DIAGS,
        );
        assert_eq!(bare, "?: error: aborting due to 2 previous errors");
        // Cap + omission note.
        let many: Vec<Diagnostic> = (0..MAX_DIAGS + 3)
            .map(|i| Diagnostic {
                level: "error".into(),
                message: format!("e{i}"),
                file: Some("f.rs".into()),
                line: Some(1),
                column: Some(1),
            })
            .collect();
        let capped = render(&many, MAX_DIAGS);
        assert!(capped.contains("3 more diagnostics omitted"), "{capped}");
        assert_eq!(capped.lines().count(), MAX_DIAGS + 1);
    }

    #[test]
    fn missing_toolchain_fails_open_with_a_note() {
        let dir = tmpdir();
        let out = collect(&dir, "overseer-no-such-binary-xyz", &["check"], "error");
        assert!(
            !out.is_error,
            "a missing binary must not error: {}",
            out.text
        );
        assert!(out.text.contains("unavailable"), "{}", out.text);
        assert!(out.text.contains("fail-open"), "{}", out.text);
    }

    #[test]
    fn real_stream_parses_through_the_tool_path() {
        // Drive the tool with a stand-in program that prints the fixture:
        // the whole path (spawn → temp file → parse → render) is exercised.
        let dir = tmpdir();
        let fixture = dir.join("stream.jsonl");
        std::fs::write(&fixture, STREAM).unwrap();
        let out = collect(
            &dir,
            "sh",
            &["-c", &format!("cat {}", fixture.display())],
            "error",
        );
        assert!(!out.is_error);
        assert!(
            out.text
                .contains("src/helper.rs:40:1: error: mismatched types"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("unused variable"), "warnings filtered");

        // The tool's argv policy (no toolchain spawn in the suite: `collect`
        // above already covers spawn → parse → render end to end).
        assert_eq!(
            check_argv(None),
            vec!["check", "--message-format=json", "--quiet"]
        );
        assert_eq!(
            check_argv(Some("overseer-core")),
            vec![
                "check",
                "--message-format=json",
                "--quiet",
                "-p",
                "overseer-core"
            ]
        );
        let _ = ctx(&dir);
    }
}
