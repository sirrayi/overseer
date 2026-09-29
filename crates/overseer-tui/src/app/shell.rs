//! `!cmd` local shell escape — split out of `app.rs` (W3).

use super::*;

/// `!` in line mode (`run_line`) — `shell_capture` against the agent
/// cwd (`--cwd`), not wherever the shell happened to launch.
pub fn line_shell(cmd: &str, cwd: &str) -> (i32, String) {
    shell_capture(cmd, cwd).unwrap_or((-1, "shell failed\n".to_string()))
}

/// Run `sh -c cmd` in `cwd` with a 10 s cap; returns (exit, capped
/// output). The `!` composer prefix is a local escape hatch — its
/// output is transcript-only, never submitted to the model.
pub(crate) fn shell_capture(cmd: &str, cwd: &str) -> std::io::Result<(i32, String)> {
    shell_capture_timeout(cmd, cwd, Duration::from_secs(10))
}

/// The cap is a parameter so tests don't wait 10 s. On timeout the
/// child is killed AND reaped — the old version left `sleep`-style
/// children running past the cap.
pub(crate) fn shell_capture_timeout(
    cmd: &str,
    cwd: &str,
    cap: Duration,
) -> std::io::Result<(i32, String)> {
    // A stale cwd (deleted checkout) must not kill the shell escape —
    // fall back to the process cwd.
    let dir = if std::path::Path::new(cwd).is_dir() {
        cwd
    } else {
        "."
    };
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    // Reader threads drain the pipes so a chatty child never
    // blocks on a full buffer while we poll try_wait.
    let mut out_pipe = child.stdout.take();
    let mut err_pipe = child.stderr.take();
    let out_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = out_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let err_t = std::thread::spawn(move || {
        let mut v = Vec::new();
        if let Some(p) = err_pipe.as_mut() {
            let _ = p.read_to_end(&mut v);
        }
        v
    });
    let deadline = Instant::now() + cap;
    // None = hit the cap: kill + reap so the child can't outlive us.
    let code = loop {
        match child.try_wait()? {
            Some(status) => break Some(status.code().unwrap_or(-1)),
            None if Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let stdout = out_t.join().unwrap_or_default();
    let stderr = err_t.join().unwrap_or_default();
    match code {
        Some(code) => {
            let mut text = String::from_utf8_lossy(&stdout).into_owned();
            let err = String::from_utf8_lossy(&stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            Ok((code, text.chars().take(8192).collect()))
        }
        None => Ok((-1, format!("(timed out after {cap:?})"))),
    }
}

impl App {
    /// `!cmd`: run in the workspace shell, output lands in the
    /// transcript — never sent to the model. 10 s cap, 8 KiB output.
    pub(crate) fn run_shell(&mut self, cmd: &str) {
        self.pending.push(Cell::Meta {
            style: crate::theme::meta(),
            text: format!("$ {cmd}"),
        });
        match shell_capture(cmd, &self.cwd) {
            Ok((code, out)) => {
                let tail: String = out.lines().take(24).collect::<Vec<_>>().join("\n");
                let suffix = if out.len() > 8192 { "…" } else { "" };
                self.pending.push(Cell::Meta {
                    style: if code == 0 {
                        crate::theme::dim()
                    } else {
                        crate::theme::error()
                    },
                    text: format!("{tail}{suffix}\n(exit {code})"),
                });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: format!("shell failed: {e}"),
            }),
        }
    }
}
