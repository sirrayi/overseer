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
    // P8-C: an explicit `--runtime` (gVisor port) PINS the backend. An
    // unavailable runtime fails the call with the requirement spelled out —
    // a run that asked for gVisor must never execute unsandboxed because
    // `runsc` was missing.
    let (prog, args, note) = match ctx
        .agent_config
        .as_ref()
        .and_then(|c| c.sandbox_runtime.as_deref())
    {
        Some(requested) => match pinned_wrap(requested, command, ctx) {
            Ok(v) => v,
            Err(e) => return ToolOutput::err(e),
        },
        None => wrap_command(command, ctx),
    };

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
    if let Some(inv) = seatbelt_invocation(command, ctx) {
        return inv;
    }
    #[cfg(target_os = "linux")]
    if let Some(inv) = bubblewrap_invocation(command, ctx) {
        return inv;
    }
    (
        "sh".into(),
        vec!["-c".into(), command.into()],
        Some("no sandbox backend (sandbox-exec/bwrap) — ran unsandboxed".into()),
    )
}

/// The macOS seatbelt invocation, or `None` when sandbox-exec is absent.
/// One builder for both the platform default path and the pinned
/// `--runtime seatbelt` path, so the two can never drift.
#[cfg(target_os = "macos")]
fn seatbelt_invocation(
    command: &str,
    ctx: &ToolCtx,
) -> Option<(String, Vec<String>, Option<String>)> {
    let exe = "/usr/bin/sandbox-exec";
    std::path::Path::new(exe).exists().then(|| {
        (
            exe.into(),
            vec![
                "-p".into(),
                macos_profile(&ctx.cwd),
                "sh".into(),
                "-c".into(),
                command.into(),
            ],
            None,
        )
    })
}

/// The bwrap invocation, or `None` when bwrap is unavailable. Same reasoning
/// as `seatbelt_invocation`: one builder, two callers.
#[cfg(target_os = "linux")]
fn bubblewrap_invocation(
    command: &str,
    ctx: &ToolCtx,
) -> Option<(String, Vec<String>, Option<String>)> {
    if !bwrap_available() {
        return None;
    }
    let root = ctx.cwd.canonicalize().unwrap_or_else(|_| ctx.cwd.clone());
    Some((
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
    ))
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

/// The pinned-runtime path (P8-C `--runtime`): build the invocation for the
/// runtime the operator asked for, or fail with the requirement spelled out.
///
/// The contract is deliberately unforgiving: an unknown name, a runtime this
/// platform cannot provide, or a missing binary is an ERROR naming what is
/// missing and how to proceed. Silently falling back to the platform default
/// (or to unsandboxed exec) would make `--runtime` a lie — and a sandbox that
/// quietly is not there is worse than no sandbox at all, because the operator
/// stops checking.
fn pinned_wrap(
    requested: &str,
    command: &str,
    ctx: &ToolCtx,
) -> Result<(String, Vec<String>, Option<String>), String> {
    let runtime = crate::backends::SandboxRuntime::parse(requested)
        .map_err(|e| format!("bash: {e} — drop --runtime to use the platform default"))?;
    match runtime {
        crate::backends::SandboxRuntime::Native => Ok((
            "sh".into(),
            vec!["-c".into(), command.into()],
            Some(
                "runtime=native — running unsandboxed because --runtime native was requested"
                    .into(),
            ),
        )),
        crate::backends::SandboxRuntime::Seatbelt => {
            #[cfg(target_os = "macos")]
            {
                seatbelt_invocation(command, ctx).ok_or_else(|| {
                    "bash: --runtime seatbelt needs /usr/bin/sandbox-exec, which is not present \
                     on this host — drop --runtime or pass --runtime native"
                        .to_string()
                })
            }
            #[cfg(not(target_os = "macos"))]
            {
                Err(
                    "bash: --runtime seatbelt is macOS-only — use --runtime bubblewrap on \
                     Linux, or --runtime native"
                        .to_string(),
                )
            }
        }
        crate::backends::SandboxRuntime::Bubblewrap => {
            #[cfg(target_os = "linux")]
            {
                bubblewrap_invocation(command, ctx).ok_or_else(|| {
                    "bash: --runtime bubblewrap needs `bwrap` on PATH (and a user namespace \
                     it can create) — drop --runtime or pass --runtime native"
                        .to_string()
                })
            }
            #[cfg(not(target_os = "linux"))]
            {
                Err(
                    "bash: --runtime bubblewrap is Linux-only — use --runtime seatbelt on \
                     macOS, or --runtime native"
                        .to_string(),
                )
            }
        }
        crate::backends::SandboxRuntime::Gvisor => {
            let bundle = ctx.cwd.join(crate::backends::GVISOR_BUNDLE_DIR).is_dir();
            crate::backends::check_runtime(runtime, binary_available("runsc"), bundle)
                .map_err(|e| format!("bash: --runtime gvisor — {e}"))?;
            // Both requirements are met, but the runsc invocation itself is
            // not wired: `runsc run` needs the prepared OCI bundle to be
            // mounted with the workspace, and guessing that argv would be a
            // fake sandbox. Refuse loudly instead.
            Err(
                "bash: --runtime gvisor — runsc and the OCI bundle are present, but the runsc \
                 invocation is not wired yet (DEFERRED, see backends.rs) — pass --runtime native \
                 to run unsandboxed or drop --runtime"
                    .to_string(),
            )
        }
    }
}

/// Whether `name` answers `--version` — the presence probe for a sandbox
/// binary (`bwrap`, `runsc`). A binary that cannot be spawned, or that exits
/// non-zero, counts as ABSENT: a runtime gate must not be satisfied by a name
/// that resolves to nothing runnable.
fn binary_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Whether bwrap exists on PATH — probed once per process.
#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    static FOUND: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| binary_available("bwrap"));
    *FOUND
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

    // ── P8-C `--runtime` (gVisor port) ───────────────────────────────────

    /// A ctx whose agent config pins the sandbox runtime, as `--runtime`
    /// does. The pin lives on the config (not on ToolCtx), so this is the
    /// exact shape a real run carries.
    fn ctx_with_runtime<'a>(dir: &'a std::path::Path, runtime: &str) -> ToolCtx<'a> {
        let mut c = ctx(dir, true);
        c.agent_config = Some(crate::agent::AgentConfig {
            sandbox_runtime: Some(runtime.to_string()),
            ..Default::default()
        });
        c
    }

    #[test]
    fn pinned_native_runs_unsandboxed_only_because_it_was_asked_for() {
        let dir = std::env::temp_dir();
        let c = ctx_with_runtime(&dir, "native");
        let (prog, args, note) = pinned_wrap("native", "echo hi", &c).unwrap();
        assert_eq!(prog, "sh");
        assert_eq!(args, vec!["-c", "echo hi"]);
        let note = note.expect("an unsandboxed run must be visible, never quiet");
        assert!(note.contains("native"), "{note}");
    }

    #[test]
    fn pinned_unknown_runtime_errors_naming_the_valid_set() {
        let dir = std::env::temp_dir();
        let c = ctx(&dir, true);
        let err = pinned_wrap("firecracker", "echo hi", &c).unwrap_err();
        assert!(err.contains("firecracker"), "{err}");
        assert!(err.contains("gvisor"), "names the valid set: {err}");
        assert!(err.contains("--runtime"), "names the flag to fix: {err}");
    }

    #[test]
    fn pinned_gvisor_without_runsc_fails_instead_of_running_unsandboxed() {
        // Both requirements are probed for real: on a host with runsc and a
        // bundle the call still refuses (the runsc argv is DEFERRED), and on
        // a host without them it names what is missing. Either way it is an
        // ERROR — never a silent downgrade to unsandboxed exec.
        let dir = std::env::temp_dir().join("ovr-sbx-gvisor");
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = ctx_with_runtime(&dir, "gvisor");
        let err = pinned_wrap("gvisor", "echo hi", &c).unwrap_err();
        assert!(err.contains("gvisor"), "{err}");
        assert!(
            err.contains("runsc") || err.contains("bundle"),
            "names the missing requirement: {err}"
        );
        let mut reg = ToolRegistry::core(Policy::allow_all());
        let out = reg.call("bash", &serde_json::json!({"command": "echo hi"}), &mut c);
        assert!(out.is_error, "a pinned-but-unavailable runtime is an error");
        assert!(out.text.contains("gvisor"), "{}", out.text);
    }

    #[test]
    fn pinned_runtime_is_platform_checked_and_the_native_path_agrees() {
        let dir = std::env::temp_dir();
        let c = ctx(&dir, true);
        #[cfg(target_os = "macos")]
        {
            let err = pinned_wrap("bubblewrap", "echo hi", &c).unwrap_err();
            assert!(err.contains("Linux-only"), "{err}");
            assert!(err.contains("native"), "offers the repair: {err}");
            // Pinning seatbelt is the default path, and both agree.
            match pinned_wrap("seatbelt", "echo hi", &c) {
                Ok((prog, _, _)) => assert!(prog.ends_with("sandbox-exec"), "{prog}"),
                Err(e) => assert!(e.contains("sandbox-exec"), "only absence explains it: {e}"),
            }
        }
        #[cfg(target_os = "linux")]
        {
            let err = pinned_wrap("seatbelt", "echo hi", &c).unwrap_err();
            assert!(err.contains("macOS-only"), "{err}");
        }
    }

    #[test]
    fn a_pinned_native_run_still_executes_the_command() {
        let dir = std::env::temp_dir().join("ovr-sbx-pinned-run");
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = ctx_with_runtime(&dir, "native");
        let mut reg = ToolRegistry::core(Policy::allow_all());
        let out = reg.call(
            "bash",
            &serde_json::json!({"command": "echo pinned-ok"}),
            &mut c,
        );
        assert!(out.text.contains("pinned-ok"), "{}", out.text);
        assert!(!out.is_error, "{}", out.text);
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
