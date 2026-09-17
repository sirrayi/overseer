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

    // P1.5 sandbox v1: the command still runs via `sh -c`, but wrapped in
    // the platform sandbox when available (macOS sandbox-exec / Linux
    // bwrap) — deny-by-default network, writes confined to the workspace.
    let (prog, args, note) = wrap_command(command, ctx);

    let mut child = match Command::new(&prog)
        .args(&args)
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
        // P6-3 broker injection: declared secrets enter the child's env
        // (selector → real). The model never sees these values — tool
        // results sanitize back to sentinels in `call()`.
        .envs(broker_env(ctx))
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

    let text = match note {
        Some(n) => format!("{status_text}\n[{n}]\n{body}"),
        None => format!("{status_text}\n{body}"),
    };
    let mut o = ToolOutput::ok(text);
    o.is_error = is_error;
    o
}

/// Declared broker secrets as (selector, real) env pairs for the child.
/// Empty without a broker — the allowlisted env above is untouched.
fn broker_env(ctx: &ToolCtx) -> Vec<(String, String)> {
    ctx.broker
        .as_ref()
        .map(|br| br.inject_env())
        .unwrap_or_default()
}

/// Pick the exec backend for a bash call. Returns (program, argv, warning):
/// sandbox-exec on macOS, bwrap on Linux, plain `sh` when sandboxing is off
/// or no backend exists (with an honest note so the model/user can see it).
fn wrap_command(command: &str, ctx: &ToolCtx) -> (String, Vec<String>, Option<String>) {
    if !ctx.sandbox {
        return ("sh".into(), vec!["-c".into(), command.into()], None);
    }
    #[cfg(target_os = "macos")]
    {
        let exe = "/usr/bin/sandbox-exec";
        if std::path::Path::new(exe).exists() {
            return (
                exe.into(),
                vec![
                    "-p".into(),
                    macos_profile(&ctx.cwd),
                    "sh".into(),
                    "-c".into(),
                    command.into(),
                ],
                None,
            );
        }
    }
    #[cfg(target_os = "linux")]
    {
        if bwrap_available() {
            let root = ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone());
            return (
                "bwrap".into(),
                vec![
                    "--ro-bind".into(),
                    "/".into(),
                    "/".into(),
                    "--bind".into(),
                    root.display().to_string(),
                    root.display().to_string(),
                    "--tmpfs".into(),
                    "/tmp".into(),
                    "--dev".into(),
                    "/dev".into(),
                    "--proc".into(),
                    "/proc".into(),
                    "--unshare-net".into(),
                    "--die-with-parent".into(),
                    "sh".into(),
                    "-c".into(),
                    command.into(),
                ],
                None,
            );
        }
    }
    (
        "sh".into(),
        vec!["-c".into(), command.into()],
        Some("no sandbox backend (sandbox-exec/bwrap) — ran unsandboxed".into()),
    )
}

/// macOS Seatbelt profile for `sandbox-exec -p` (P1.5): deny-by-default,
/// exec/read freely, writes only to the workspace + temp dirs, network
/// fully denied (deny overrides allow regardless of order). Secret dirs
/// are read-denied on top of the broad read allow. A loopback egress
/// proxy with domain allowlists is still open — v1 denies all egress.
#[cfg(target_os = "macos")]
fn macos_profile(cwd: &std::path::Path) -> String {
    let root = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let home = std::env::var("HOME").unwrap_or_default();
    format!(
        "(version 1)\n\
         (deny default)\n\
         (allow process-exec process-fork)\n\
         (allow signal (target self))\n\
         (allow process-info*)\n\
         (allow sysctl-read mach-lookup ipc-posix-shm)\n\
         (allow file-read*)\n\
         (allow file-write* (subpath \"{root}\") (subpath \"/private/tmp\") \
         (subpath \"/private/var\") (literal \"/dev/null\") (literal \"/dev/tty\"))\n\
         (deny file-read* (subpath \"{home}/.ssh\") (subpath \"{home}/.aws\") \
         (subpath \"{home}/.gnupg\") (subpath \"{home}/.kube\") (subpath \"{home}/.docker\"))\n\
         (deny network*)\n",
        root = root.display(),
        home = home
    )
}

/// Whether bwrap exists on PATH — probed once per process.
#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FOUND.get_or_init(|| {
        Command::new("bwrap")
            .arg("--version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perm::Policy;
    use crate::tools::{ToolCtx, ToolRegistry};

    fn ctx(dir: &std::path::Path, sandbox: bool) -> ToolCtx<'_> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("s"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: None,
            sandbox,
            broker: None,
        }
    }

    #[test]
    fn sandbox_off_wraps_plain_sh() {
        let dir = std::env::temp_dir();
        let (prog, args, note) = wrap_command("echo hi", &ctx(&dir, false));
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
        assert!(note.is_none());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_sandbox_wraps_sandbox_exec_with_confinement() {
        let dir = std::env::temp_dir().join("ovr-sbx-test");
        std::fs::create_dir_all(&dir).unwrap();
        let (prog, args, note) = wrap_command("echo hi", &ctx(&dir, true));
        assert!(prog.ends_with("sandbox-exec"));
        let profile = &args[1];
        assert!(profile.contains("(deny network*)"));
        // Canonicalized workspace root is the write-allowed subpath.
        let root = dir.canonicalize().unwrap();
        assert!(profile.contains(&format!("subpath \"{}\"", root.display())));
        assert_eq!(args.last().unwrap(), "echo hi");
        assert!(note.is_none());
    }

    #[test]
    fn sandboxed_exec_runs_and_confines() {
        // Only meaningful on macOS (or Linux with bwrap) — skip silently
        // where no backend exists.
        let dir = std::env::temp_dir().join("ovr-sbx-run");
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = ctx(&dir, true);
        let (prog, _, note) = wrap_command("true", &c);
        if note.is_some() {
            return; // no sandbox backend on this host
        }
        let _ = prog;
        let mut reg = ToolRegistry::core(Policy::allow_all());
        let out = reg.call(
            "bash",
            &serde_json::json!({"command": "echo ok > sbx-out.txt && cat sbx-out.txt"}),
            &mut c,
        );
        assert!(out.text.contains("ok"), "{}", out.text);
    }

    #[test]
    fn broker_env_injects_real_not_sentinel() {
        // P6-3 accept (injection): the child env carries the real via
        // inject_env; the sentinel never appears in the injected pairs.
        let mut br = crate::cred::Broker::new();
        let sentinel = br.issue_capability(
            "api",
            "API_TOKEN",
            "tok-real-123",
            vec!["api.example.com".into()],
            vec!["read".into()],
            None,
        );
        let dir = std::env::temp_dir();
        let c = ctx(&dir, false);
        assert!(broker_env(&c).is_empty());
        let mut c2 = ctx(&dir, false);
        c2.broker = Some(br);
        let env = broker_env(&c2);
        assert_eq!(
            env,
            vec![("API_TOKEN".to_string(), "tok-real-123".to_string())]
        );
        assert!(!env.iter().any(|(_, v)| v.contains(&sentinel)));
    }
}
