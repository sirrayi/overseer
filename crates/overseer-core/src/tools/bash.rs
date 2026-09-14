//! bash tool — stateless exec per action (Invariant 4: mini-SWE-agent's
//! property — every action an independent subprocess, trivially sandboxable,
//! no shell desync). Persistent-shell semantics arrive later as a single
//! owned PTY with marker-based completion, not an implicit shared tty.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{middle_truncate, need_str, opt_u64, schema, ToolCtx, ToolOutput};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
const MAX_TIMEOUT_MS: u64 = 600_000;
/// Logs get middle-truncated within the inline cap (errors cluster at tail).
const LOG_TRUNC: usize = 20_000;

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "bash".into(),
        description: concat!(
            "Run a shell command in the working directory via `sh -c`. ",
            "Each call is a fresh, stateless process — no working directory or ",
            "environment carries over between calls (use absolute paths or cd && cmd). ",
            "Reserved for what the dedicated tools can't do: building, testing, git, ",
            "package managers. Output is truncated/spilled to a file when large."
        )
        .into(),
        input_schema: schema(
            json!({
                "command": {
                    "type": "string",
                    "description": "The shell command to run (sh -c)."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional timeout in milliseconds (default 120000, max 600000)."
                }
            }),
            &["command"],
        ),
    }
}

pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let command = match need_str(input, "command") {
        Ok(c) => c,
        Err(e) => return e,
    };
    let timeout = opt_u64(input, "timeout_ms")
        .unwrap_or(DEFAULT_TIMEOUT_MS)
        .min(MAX_TIMEOUT_MS);

    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(&ctx.cwd)
        .env_clear()
        .envs(std::env::vars().filter(|(k, _)| {
            // Minimal allowlisted env — secrets stay out of child scope
            // unless deliberately inherited (playbook Ch.10 §4).
            matches!(
                k.as_str(),
                "PATH"
                    | "HOME"
                    | "USER"
                    | "SHELL"
                    | "TERM"
                    | "LANG"
                    | "LC_ALL"
                    | "TMPDIR"
                    | "SSH_AUTH_SOCK"
                    | "GIT_AUTHOR_NAME"
                    | "GIT_AUTHOR_EMAIL"
                    | "GIT_COMMITTER_NAME"
                    | "GIT_COMMITTER_EMAIL"
                    | "CI"
            ) || k.starts_with("LC_")
        }))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("Failed to spawn shell: {e}")),
    };

    // Read pipes on threads to avoid pipe-buffer deadlock.
    let mut out_pipe = child.stdout.take().unwrap();
    let mut err_pipe = child.stderr.take().unwrap();
    let out_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = out_pipe.read_to_end(&mut buf);
        buf
    });
    let err_thread = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = err_pipe.read_to_end(&mut buf);
        buf
    });

    let deadline = Instant::now() + Duration::from_millis(timeout);
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    break None;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => return ToolOutput::err(format!("Failed waiting on command: {e}")),
        }
    };

    let stdout = String::from_utf8_lossy(&out_thread.join().unwrap_or_default()).into_owned();
    let stderr = String::from_utf8_lossy(&err_thread.join().unwrap_or_default()).into_owned();

    let (status_text, is_error) = match status {
        Some(s) if s.success() => (format!("exit {}", s.code().unwrap_or(0)), false),
        Some(s) => (format!("exit {}", s.code().unwrap_or(-1)), true),
        None => (format!("killed after {}ms (timeout)", timeout), true),
    };

    let mut body = String::new();
    if !stdout.is_empty() {
        body.push_str(&middle_truncate(&stdout, LOG_TRUNC));
    }
    if !stderr.is_empty() {
        if !body.is_empty() {
            body.push_str("\n--- stderr ---\n");
        }
        body.push_str(&middle_truncate(&stderr, LOG_TRUNC));
    }
    if body.is_empty() {
        body.push_str("(no output)");
    }

    let text = format!("{status_text}\n{body}");
    let mut o = ToolOutput::ok(text);
    o.is_error = is_error;
    o
}
