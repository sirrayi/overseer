//! The connector contract and the injectable context every fetch runs
//! under: home dir, env map, clock, bounded HTTP client and a command
//! runner. Everything is injectable so tests never touch the real home
//! directory, keychain, or network.

use std::collections::BTreeMap;
use std::io;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::http::Http;
use crate::snapshot::{Snapshot, SourceId};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    MacOS,
    Linux,
    Windows,
    Other,
}

impl Platform {
    pub fn current() -> Self {
        match std::env::consts::OS {
            "macos" => Platform::MacOS,
            "linux" => Platform::Linux,
            "windows" => Platform::Windows,
            _ => Platform::Other,
        }
    }

    /// Synara's `process.platform` spelling, used in ported path logic.
    pub fn is_darwin(self) -> bool {
        self == Platform::MacOS
    }

    pub fn is_windows(self) -> bool {
        self == Platform::Windows
    }
}

/// Output of a spawned command. Stdout may legitimately contain secrets
/// (a keychain read's whole point is the secret); callers must never put
/// it into errors, logs or snapshots — that invariant lives in the
/// connectors.
#[derive(Debug)]
pub struct CmdOutput {
    /// Exit code; -1 when the process was killed or never exited.
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// Injectable process boundary. `SystemRunner` spawns for real; tests
/// substitute a fake that records argv and returns canned output. Never
/// put secrets on argv — the command line is world-visible while the
/// process runs.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[&str], timeout_ms: u64) -> io::Result<CmdOutput>;
}

/// Real process spawning with a wall-clock timeout (the child is polled
/// with `try_wait` and killed past the deadline — a `security` dialog or
/// a wedged `sqlite3` cannot hang the caller). Only used by the probe
/// binary; the library is runner-agnostic.
pub struct SystemRunner;

impl CommandRunner for SystemRunner {
    fn run(&self, program: &str, args: &[&str], timeout_ms: u64) -> io::Result<CmdOutput> {
        use std::process::{Command, Stdio};

        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;

        // Pipe-drain threads prevent a full pipe from deadlocking the wait.
        let mut out_pipe = child.stdout.take();
        let mut err_pipe = child.stderr.take();
        let stdout_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(pipe) = out_pipe.as_mut() {
                use std::io::Read;
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });
        let stderr_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(pipe) = err_pipe.as_mut() {
                use std::io::Read;
                let _ = pipe.read_to_end(&mut buf);
            }
            buf
        });

        let deadline =
            std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms.max(1));
        loop {
            match child.try_wait()? {
                Some(status) => {
                    let stdout = stdout_thread.join().unwrap_or_default();
                    let stderr = stderr_thread.join().unwrap_or_default();
                    return Ok(CmdOutput {
                        status: status.code().unwrap_or(-1),
                        stdout,
                        stderr,
                    });
                }
                None if std::time::Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    // Reap the drain threads so no zombie work outlives
                    // the error return.
                    let _ = stdout_thread.join();
                    let _ = stderr_thread.join();
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("command timed out after {timeout_ms} ms"),
                    ));
                }
                None => std::thread::sleep(std::time::Duration::from_millis(10)),
            }
        }
    }
}

/// A credential a connector can see, for discovery-only output. The
/// location is a path or `keychain:<service>` — never a secret value.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Discovered {
    /// "file", "env", "keychain"
    pub kind: &'static str,
    /// Path, env var name, or keychain service — metadata only.
    pub location: String,
    /// What is expected there ("oauth access token", "api key").
    pub contains: &'static str,
    /// Whether it actually yielded a usable credential at discovery time.
    pub present: bool,
}

/// Static self-description.
pub struct ConnectorInfo {
    pub id: &'static str,
    pub name: &'static str,
    /// True when fetch() may hit the network (archives are false).
    pub needs_network: bool,
}

pub trait Connector {
    fn id(&self) -> SourceId;
    fn info(&self) -> ConnectorInfo;
    /// Credential/archive discovery — read-only filesystem and
    /// attribute-only keychain checks. Never returns secret material.
    fn discover(&self, ctx: &Ctx) -> Vec<Discovered>;
    /// Resolve credentials and fetch. Never panics; the snapshot's status
    /// carries every outcome including malformed input.
    fn fetch(&self, ctx: &Ctx) -> Snapshot;
}

/// Everything a fetch can touch, all injectable.
pub struct Ctx {
    pub home_dir: PathBuf,
    pub env: BTreeMap<String, String>,
    pub now_ms: i64,
    pub platform: Platform,
    pub http: Http,
    pub runner: Box<dyn CommandRunner>,
    /// Ported from Synara `isolateCredentials`: when set, connectors only
    /// consult the explicit override locations (env-pointed dirs) and skip
    /// ambient fallbacks like the default home-dir paths and keychain.
    pub isolate_credentials: bool,
    /// R3 latch: when false, keychain SECRET reads (the `-w` form of
    /// `security`) are skipped entirely — connectors see them as absent.
    /// Attribute-only existence checks are unaffected. The probe keeps
    /// this off; a host enables it deliberately.
    pub allow_keychain_secrets: bool,
}

impl Ctx {
    pub fn env(&self, key: &str) -> Option<&str> {
        self.env
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
    }

    pub fn now_ms() -> i64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0)
    }

    /// Build a ctx for the real environment (probe binary).
    pub fn system(http: Http, allow_keychain_secrets: bool) -> Self {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."));
        Ctx {
            home_dir: home,
            env: std::env::vars().collect(),
            now_ms: Self::now_ms(),
            platform: Platform::current(),
            http,
            runner: Box::new(SystemRunner),
            isolate_credentials: false,
            allow_keychain_secrets,
        }
    }

    /// Test ctx: fixed home/clock, fake runner, loopback-capable HTTP.
    #[doc(hidden)]
    pub fn for_test(home: PathBuf, runner: Box<dyn CommandRunner>) -> Self {
        Ctx {
            home_dir: home,
            env: BTreeMap::new(),
            now_ms: 1_800_000_000_000, // 2027-01-15
            platform: Platform::MacOS,
            http: Http::for_test(),
            runner,
            isolate_credentials: false,
            allow_keychain_secrets: false,
        }
    }

    /// Test hook: point a connector's endpoint at a mock server. Only
    /// honored when loopback http is enabled, so it can never redirect a
    /// production fetch.
    pub fn override_url(&self, key: &'static str) -> Option<String> {
        if !self.http.allow_loopback_http {
            return None;
        }
        self.env(&format!("OVS_LIFE_URL_{key}")).map(str::to_string)
    }

    /// `(url, allowed_origin)` for a connector call — the constant
    /// endpoint normally, or the mock override (with its origin) in tests.
    pub fn endpoint(&self, key: &'static str, default_url: &str) -> (String, String) {
        if let Some(url) = self.override_url(key) {
            let origin = crate::http::origin_of(&url).unwrap_or_default();
            return (url, origin);
        }
        (
            default_url.to_string(),
            crate::http::origin_of(default_url).unwrap_or_default(),
        )
    }

    /// `Some(path)` joined under home_dir.
    pub fn home(&self, parts: &[&str]) -> PathBuf {
        let mut p = self.home_dir.clone();
        for part in parts {
            p.push(part);
        }
        p
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Real processes on purpose: the contract being pinned is that the
    // timeout actually fires against a child that never exits.
    #[cfg(unix)]
    #[test]
    fn system_runner_kills_a_hung_child() {
        let started = std::time::Instant::now();
        let err = SystemRunner
            .run("/bin/sleep", &["5"], 200)
            .expect_err("sleep 5 must time out");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "timeout should kill the child well under 2s: {:?}",
            started.elapsed()
        );
    }

    #[cfg(unix)]
    #[test]
    fn system_runner_collects_stdout() {
        let out = SystemRunner
            .run("/bin/echo", &["hi"], 5_000)
            .expect("echo runs");
        assert_eq!(out.status, 0);
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    }
}
