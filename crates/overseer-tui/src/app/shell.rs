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
    shell_capture_timeout(cmd, cwd, SHELL_CAP)
}

const SHELL_CAP: Duration = Duration::from_secs(10);

/// How long the reader joins wait for EOF once the group is dead.
const JOIN_GRACE: Duration = Duration::from_millis(500);

const BG_NOTE: &str = "(still running in the background)";

/// The cap is a parameter so tests don't wait 10 s. The child gets its
/// own process group and the whole group is killed on completion or
/// timeout; a process that left the group (`setsid`, double fork) and
/// still holds the pipes is reported as running in the background.
pub(crate) fn shell_capture_timeout(
    cmd: &str,
    cwd: &str,
    cap: Duration,
) -> std::io::Result<(i32, String)> {
    let c = capture(cmd, cwd, cap)?;
    let mut text = c.text;
    if c.background {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str(BG_NOTE);
        text.push('\n');
    }
    Ok((c.code, text))
}

pub(crate) struct Captured {
    pub code: i32,
    pub text: String,
    /// A process outside the group still holds stdout/stderr.
    pub background: bool,
}

/// Drain one pipe into `buf`, signalling `done` at EOF.
fn drain(
    pipe: Option<impl Read + Send + 'static>,
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    done: mpsc::Sender<()>,
) {
    std::thread::spawn(move || {
        if let Some(mut p) = pipe {
            let mut chunk = [0u8; 4096];
            loop {
                match p.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        }
        let _ = done.send(());
    });
}

pub(crate) fn capture(cmd: &str, cwd: &str, cap: Duration) -> std::io::Result<Captured> {
    // A stale cwd (deleted checkout) must not kill the shell escape —
    // fall back to the process cwd.
    let dir = if std::path::Path::new(cwd).is_dir() {
        cwd
    } else {
        "."
    };
    use std::os::unix::process::CommandExt as _;
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // Own process group: dash keeps `sleep`-style children (and any
        // pipeline or `&` job) as separate processes, so killing only
        // `sh` orphans grandchildren that hold the pipe write ends.
        .process_group(0)
        .spawn()?;
    let pgid = child.id() as libc::pid_t;
    // Reader threads drain the pipes so a chatty child never blocks
    // on a full buffer while we poll try_wait. They report through a
    // channel so the join below can give up on a pipe an escaped
    // process keeps open.
    let out_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let err_buf = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    drain(child.stdout.take(), out_buf.clone(), done_tx.clone());
    drain(child.stderr.take(), err_buf.clone(), done_tx);
    let deadline = Instant::now() + cap;
    // None = hit the cap.
    let code = loop {
        match child.try_wait()? {
            Some(status) => {
                // `sh` is done; anything it left in the group (`cmd &`)
                // goes with it so the pipes can close.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                break Some(status.code().unwrap_or(-1));
            }
            None if Instant::now() >= deadline => {
                // Kill the group before reaping `sh` (its pid can't be
                // reused while unreaped); kill the child as a fallback.
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            None => std::thread::sleep(Duration::from_millis(5)),
        }
    };
    let now = Instant::now();
    let join_by = if code.is_some() {
        (now + JOIN_GRACE).min(deadline.max(now))
    } else {
        now + JOIN_GRACE
    };
    let mut open = 2;
    while open > 0 {
        let rem = join_by.saturating_duration_since(Instant::now());
        match done_rx.recv_timeout(rem) {
            Ok(()) => open -= 1,
            Err(_) => break,
        }
    }
    let background = open > 0;
    let stdout = std::mem::take(&mut *out_buf.lock().unwrap());
    let stderr = std::mem::take(&mut *err_buf.lock().unwrap());
    let text = match code {
        Some(_) => {
            let mut text = String::from_utf8_lossy(&stdout).into_owned();
            let err = String::from_utf8_lossy(&stderr);
            if !err.trim().is_empty() {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(&err);
            }
            text.chars().take(8192).collect()
        }
        None => format!("(timed out after {cap:?})"),
    };
    Ok(Captured {
        code: code.unwrap_or(-1),
        text,
        background,
    })
}

impl App {
    /// `!cmd`: run in the workspace shell, output lands in the
    /// transcript — never sent to the model. 10 s cap, 8 KiB output.
    pub(crate) fn run_shell(&mut self, cmd: &str) {
        self.pending.push(Cell::Meta {
            style: crate::theme::meta(),
            text: format!("$ {cmd}"),
            link: None,
        });
        match capture(cmd, &self.cwd, SHELL_CAP) {
            Ok(Captured {
                code,
                text: out,
                background,
            }) => {
                let tail: String = out.lines().take(24).collect::<Vec<_>>().join("\n");
                let suffix = if out.len() > 8192 { "…" } else { "" };
                let bg = if background {
                    format!("\n{BG_NOTE}")
                } else {
                    String::new()
                };
                self.pending.push(Cell::Meta {
                    style: if code == 0 {
                        crate::theme::dim()
                    } else {
                        crate::theme::error()
                    },
                    text: format!("{tail}{suffix}{bg}\n(exit {code})"),
                    link: None,
                });
            }
            Err(e) => self.pending.push(Cell::Meta {
                style: crate::theme::error(),
                text: format!("shell failed: {e}"),
                link: None,
            }),
        }
    }
}
