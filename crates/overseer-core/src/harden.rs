//! Startup hardening: process posture before any key or network use.
//!
//! Fail-closed and std-only (no new deps): umask + proxy-env scrub +
//! owner-only session dirs. Non-goals (per slice brief): no network
//! changes, no keychain writes, no proxy runtime — the loopback egress
//! proxy itself is still Phase-C (`backends.rs`).

use std::path::Path;

/// Proxy env vars scrubbed at startup. Lowercase + uppercase + `ALL_PROXY`
/// (covers `HTTP_PROXY`/`http_proxy`, `HTTPS_PROXY`/`https_proxy`,
/// `ALL_PROXY`/`all_proxy`, `NO_PROXY`/`no_proxy`). `ureq` honors the
/// process proxy env, so unsetting here (not just documenting) is what
/// keeps provider traffic off a planted egress proxy.
pub const PROXY_ENV_VARS: &[&str] = &[
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "ALL_PROXY",
    "all_proxy",
    "NO_PROXY",
    "no_proxy",
];

#[cfg(unix)]
extern "C" {
    fn umask(mask: u32) -> u32;
}

#[cfg(unix)]
fn libc_umask(mask: u32) -> u32 {
    // `extern` fns are unsafe to call: the signature is our claim about
    // libc, so the compiler cannot check it. umask always succeeds.
    unsafe { umask(mask) }
}

/// Harden process posture at startup. Call once from the CLI entry point
/// before providers, tools, or sessions are constructed.
///
/// - unix: set umask `0o077` (new files/dirs owner-only) via libc.
/// - Remove proxy env vars from the process (see [`PROXY_ENV_VARS`]).
///   Provider keys (`*_API_KEY`) are NOT scrubbed here — scrubbing the
///   key out of our own env would break auth; the note below documents
///   the rule.
/// - Session-dir perms are enforced by [`ensure_private_dir`], not here
///   (this fn never touches the filesystem).
///
/// Provider-key note: keys live only in process memory once read; never
/// log them, never persist them outside the OS keychain path (`cred.rs`);
/// child tool envs are scrubbed separately at spawn.
///
/// Idempotent: safe to call more than once (umask set + vars removed).
// FIXED SINCE (P9): keychain-backed provider keys — key resolution falls
// back to the credential payload (broker.real_for) in main.rs, and bash
// children already spawn env-clear + allowlist (bash.rs) so no key leaks
// into tool scope.
pub fn harden_startup() {
    #[cfg(unix)]
    {
        // umask 0o077: files 0600, dirs 0700. No new crate — raw
        // `extern "C"` link against the already-linked system libc.
        libc_umask(0o077);
    }
    for var in PROXY_ENV_VARS {
        std::env::remove_var(var);
    }
}

/// Create `dir` (and parents) and enforce owner-only perms.
///
/// - Creates with `create_dir_all`, then sets `0o700` on `dir` itself
///   (umask may already cover it — this pins it explicitly).
/// - unix: best-effort chmod, ignores errors (returns Ok anyway when the
///   dir exists; callers that need strictness check `is_private_dir`).
/// - non-unix: create-only (no chmod primitive worth gating on).
///
/// Fails only when the dir cannot be created.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

/// True when `dir` exists and is owner-only (unix: mode bits `0o700`
/// masked to `0o777` equal exactly `0o700`; non-unix: existence check).
pub fn is_private_dir(dir: &Path) -> bool {
    if !dir.is_dir() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        matches!(
            std::fs::metadata(dir).map(|m| m.permissions().mode() & 0o777),
            Ok(0o700)
        )
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("overseer-harden-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn harden_startup_scrubs_proxy_env() {
        std::env::set_var("HTTP_PROXY", "http://planted:8080");
        std::env::set_var("http_proxy", "http://planted:8080");
        std::env::set_var("ALL_PROXY", "socks5://planted:1080");
        harden_startup();
        assert!(std::env::var_os("HTTP_PROXY").is_none());
        assert!(std::env::var_os("http_proxy").is_none());
        assert!(std::env::var_os("ALL_PROXY").is_none());
        // Idempotent: second call is a no-op, not a panic.
        harden_startup();
    }

    #[test]
    fn ensure_private_dir_is_owner_only() {
        let root = tmpdir("ensure");
        let dir = root.join("session-new");
        ensure_private_dir(&dir).unwrap();
        assert!(dir.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o700, "session dir must be 0700, got {mode:o}");
        }
        assert!(is_private_dir(&dir));
        assert!(!is_private_dir(&root.join("missing")));
    }
}
