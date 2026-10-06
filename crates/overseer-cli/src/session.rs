//! Session plumbing shared by every engine-backed command: where the
//! session lives, the run config built from flags, and credential loading.

use overseer_core::memory::Scope;
use std::path::{Path, PathBuf};

use crate::flags::ExecFlags;

/// Session directory + whether to resume it. Precedence: --resume >
/// --continue (cwd-scoped) > --last > --session > fresh timestamped dir.
/// `--bare` never lands in ~/.overseer — the session is a throwaway made
/// by [`bare_session_dir`] (the only fallible branch).
pub(crate) fn resolve_session(flags: &ExecFlags) -> std::io::Result<(PathBuf, bool)> {
    if flags.bare {
        return Ok((bare_session_dir(&std::env::temp_dir())?, false));
    }
    Ok(resolve_persistent(flags))
}

/// `--bare` session dir: `<tmp>/overseer-bare-<16 hex>` from the OS RNG,
/// made by a non-recursive `create_dir` at 0700. An existing name is
/// never adopted (on a shared /tmp anyone could have planted it): a
/// fresh name is drawn instead, up to 8 tries.
pub(crate) fn bare_session_dir(tmp: &Path) -> std::io::Result<PathBuf> {
    const TRIES: usize = 8;
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    for _ in 0..TRIES {
        let mut raw = [0u8; 8];
        getrandom::fill(&mut raw).map_err(|e| std::io::Error::other(e.to_string()))?;
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        let dir = tmp.join(format!("overseer-bare-{hex}"));
        match builder.create(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        format!(
            "no free overseer-bare-* name in {} after {TRIES} tries",
            tmp.display()
        ),
    ))
}

fn resolve_persistent(flags: &ExecFlags) -> (PathBuf, bool) {
    if let Some(d) = &flags.resume {
        // F4: an explicit resume of a session open in another overseer
        // process is a usage error — two writers would fork the event
        // log's hash chain.
        if overseer_core::live::held(d) {
            eprintln!("overseer: {}", overseer_core::live::BUSY);
            std::process::exit(2);
        }
        return (d.clone(), true);
    }
    let root = dirs_home().join("sessions");
    let cwd = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    if flags.cont {
        // F4: skip sessions live elsewhere; pick the next newest free one.
        if let Some(d) = overseer_core::session::most_recent_resumable(&root, Some(&cwd)) {
            return (d, true);
        }
        eprintln!(
            "overseer: no earlier session for {} — starting fresh",
            cwd.display()
        );
    }
    if flags.last {
        if let Some(d) = overseer_core::session::most_recent_resumable(&root, None) {
            return (d, true);
        }
    }
    if let Some(d) = &flags.session {
        return (d.clone(), false);
    }
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    (dirs_home().join("sessions").join(format!("{ts}")), false)
}

pub(crate) fn agent_config(flags: &ExecFlags) -> overseer_core::agent::AgentConfig {
    let cwd_canonical = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    let (user_memory, project_memory) = memory_stores(flags, &cwd_canonical);
    // Memory v3 §10: learning is on when memory is; --bare/--no-memory
    // leave no store, so `learn` follows the stores.
    let memory_on = user_memory.is_some() || project_memory.is_some();
    overseer_core::agent::AgentConfig {
        model: flags.model.clone(),
        max_steps: flags.max_steps,
        max_cost_usd: flags.max_cost,
        max_output_tokens: 16_384,
        thinking_budget: flags.thinking,
        effort: flags.effort,
        small_model: flags.small_model.clone(),
        heavy_model: flags.heavy_model.clone(),
        max_bg_subagents: overseer_core::agent::AgentConfig::default().max_bg_subagents,
        // Canonicalize once: every subsystem (snapshots, read dedup, the
        // permission gate's containment check) assumes an absolute root —
        // a relative --cwd like "." would silently leak relative paths
        // into checkpoint manifests and policy checks.
        cwd: cwd_canonical.clone(),
        full_access: flags.full_access,
        policy_preset: flags.policy,
        auto_compact: flags.auto_compact,
        compact_at: flags.compact_at,
        memory_dir: project_memory,
        user_memory_dir: user_memory,
        memory_recall: true,
        is_subagent: false,
        learn: !flags.no_learn && !flags.bare && memory_on,
        learn_every: flags.learn_every,
        learn_stage: flags.learn_stage,
        // P6-2: the parent agent sees the full index; the ceiling applies
        // to the quarantined subagent view only.
        memory_filter: overseer_core::memory::Sensitivity::Personal,
        // P6-4: filled by `apply_credentials` (store resolution + payload).
        broker: overseer_core::cred::Broker::new(),
        keep_tool_results: flags.keep_results,
        verify_cmd: flags.verify.clone(),
        verify_block_cap: flags.verify_cap,
        sandbox_bash: flags.sandbox,
        sandbox_runtime: flags.runtime.clone(),
        disabled_tools: flags.no_tools.clone(),
        autonomy: {
            let mut m = std::collections::HashMap::new();
            for (d, l) in &flags.autonomy {
                let level = match l.as_str() {
                    "observe" => overseer_core::perm::Autonomy::Observe,
                    "suggest" => overseer_core::perm::Autonomy::Suggest,
                    "approve" => overseer_core::perm::Autonomy::ActWithApproval,
                    "report" => overseer_core::perm::Autonomy::ActAndReport,
                    _ => overseer_core::perm::Autonomy::ActSilently,
                };
                m.insert(d.clone(), level);
            }
            m
        },
        reflect: flags.reflect,
        // --bare: hermetic also covers the OS keychain — a CI run reads
        // credentials from env only, unless the operator explicitly names
        // a store (Env is the explicit-safe default; Auto would touch the
        // keychain, which is operator state, not CI state).
        credential_store: if flags.bare
            && flags.credential_store == overseer_core::cred::CredentialStore::Auto
        {
            overseer_core::cred::CredentialStore::Env
        } else {
            flags.credential_store
        },
        // P6-5: an existing <cwd>/persona dir joins the session — the
        // persona segment renders (approved bodies or the pending notice)
        // and the draft gate closes the dir to file tools until approved.
        // Same convention as --memory/<cwd>/memory.
        persona_dir: {
            let d = cwd_canonical.join("persona");
            d.is_dir().then_some(d)
        },
        // P7-1 computer-use containment: engine defaults (takeover_pause on).
        computer: Default::default(),
        ask_handler: None,
        // --bare: no persisted rules — a CI run must not inherit or
        // mutate the operator's allow-list. Ask verdicts still
        // fail-closed to deny either way.
        rules_path: if flags.bare {
            None
        } else {
            Some(dirs_home().join("rules"))
        },
        // R7: MCP servers from ~/.overseer/mcp.json. --bare is hermetic
        // (never reads operator state outside the temp session), so it
        // loads nothing; a broken config warns once and runs with no
        // servers rather than failing a startup that has nothing to do
        // with MCP.
        mcp_servers: if flags.bare {
            Vec::new()
        } else {
            load_mcp_servers()
        },
    }
}

/// R7: the configured MCP servers, or none. A missing file is the normal
/// case (no MCP configured); a malformed one is a one-line stderr warning and
/// no servers — never a crash on session start.
fn load_mcp_servers() -> Vec<overseer_core::mcp_config::McpServer> {
    let Some(path) = overseer_core::mcp_config::default_path() else {
        return Vec::new();
    };
    match overseer_core::mcp_config::load(&path) {
        Ok(servers) => servers,
        Err(e) => {
            eprintln!(
                "overseer: mcp config {}: {e} — continuing with no MCP servers",
                path.display()
            );
            Vec::new()
        }
    }
}

/// P6-4: resolve the credential store against this machine and load the
/// secrets it holds into the broker. The keychain read is prefetched so
/// its subprocess spawn overlaps the env read; the *effective* store is
/// written back onto the config, so `manifest.json` records what actually
/// held the secret (a fallback must be visible, not silent). Never prints
/// secret material — only the store and the fallback note.
pub(crate) fn apply_credentials(config: &mut overseer_core::agent::AgentConfig) {
    use overseer_core::cred;
    let configured = config.credential_store;
    let kc = cred::Keychain::detect();
    let prefetch = kc.prefetch();
    let env = cred::env_payload();
    let res = cred::resolve_store_with(configured, &kc, prefetch.resolve(), env);
    config.credential_store = res.store;
    if res.store != configured {
        eprintln!("overseer: credentials — {}", res.note);
    }
    let Some(text) = res.payload else {
        return;
    };
    match cred::parse_payload(&text) {
        Ok(p) if !p.is_empty() => {
            let (secrets, grants) = config.broker.install_payload(&p);
            if secrets > 0 || grants > 0 {
                eprintln!("overseer: credentials — {secrets} secret(s), {grants} grant(s) loaded");
            }
        }
        Ok(_) => {}
        Err(e) => eprintln!("overseer: credentials — {e} (ignored)"),
    }
}

/// Memory v2 stores `(user, project)`: on by default under the overseer
/// home; `--memory` keeps v1's in-workspace `<cwd>/memory` as the project
/// store; `--no-memory` and `--bare` turn both off.
pub(crate) fn memory_stores(flags: &ExecFlags, cwd: &Path) -> (Option<PathBuf>, Option<PathBuf>) {
    if flags.bare || flags.no_memory {
        return (None, None);
    }
    let home = overseer_core::memory::overseer_home();
    let user = home
        .as_deref()
        .map(overseer_core::memory::stores::user_store);
    let project = if flags.memory {
        Some(cwd.join("memory"))
    } else {
        home.map(|h| overseer_core::memory::stores::project_store(&h, cwd))
    };
    // I-world-writable-store: any resolved store that exists is pulled
    // back to 0700 when group/other bits crept in (e.g. an operator's
    // loose umask or a hostile chmod) — `.index/` included.
    for d in [&user, &project].into_iter().flatten() {
        if d.is_dir() {
            overseer_core::memory::tighten_perms(d);
            let idx = d.join(".index");
            if idx.is_dir() {
                overseer_core::memory::tighten_perms(&idx);
            }
        }
    }
    (user, project)
}

/// [`memory_stores`] as the scoped list the core memory APIs take.
pub(crate) fn memory_store_list(flags: &ExecFlags, cwd: &Path) -> Vec<(Scope, PathBuf)> {
    let (user, project) = memory_stores(flags, cwd);
    [(Scope::User, user), (Scope::Project, project)]
        .into_iter()
        .filter_map(|(s, d)| Some((s, d?)))
        .collect()
}

pub(crate) fn dirs_home() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
        .join(".overseer")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ovs-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn bare_dirs_are_random_private_and_fresh() {
        let t = tmp("bare");
        let a = bare_session_dir(&t).unwrap();
        let b = bare_session_dir(&t).unwrap();
        assert_ne!(a, b);
        for d in [&a, &b] {
            assert_eq!(d.parent(), Some(t.as_path()));
            let name = d.file_name().unwrap().to_str().unwrap();
            let hex = name.strip_prefix("overseer-bare-").unwrap();
            assert_eq!(hex.len(), 16, "{name}");
            assert!(hex.bytes().all(|c| c.is_ascii_hexdigit()), "{name}");
            assert_eq!(std::fs::read_dir(d).unwrap().count(), 0);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = std::fs::metadata(d).unwrap().permissions().mode();
                assert_eq!(mode & 0o777, 0o700);
            }
        }
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn bare_dir_never_creates_a_missing_parent() {
        let t = tmp("bare-missing");
        let absent = t.join("absent");
        let e = bare_session_dir(&absent).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::NotFound);
        assert!(!absent.exists());
        let _ = std::fs::remove_dir_all(&t);
    }
}
