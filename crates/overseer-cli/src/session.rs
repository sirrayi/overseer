//! Session plumbing shared by every engine-backed command: where the
//! session lives, the run config built from flags, and credential loading.

use overseer_core::memory::Scope;
use std::path::{Path, PathBuf};

use crate::flags::ExecFlags;

/// Session directory + whether to resume it. Precedence: --resume >
/// --continue (cwd-scoped) > --last > --session > fresh timestamped dir.
/// `--bare` never lands in ~/.overseer — the session is a throwaway.
pub(crate) fn resolve_session(flags: &ExecFlags) -> (PathBuf, bool) {
    if flags.bare {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        return (
            std::env::temp_dir().join(format!("overseer-bare-{ts}")),
            false,
        );
    }
    if let Some(d) = &flags.resume {
        return (d.clone(), true);
    }
    let root = dirs_home().join("sessions");
    let cwd = flags
        .cwd
        .canonicalize()
        .unwrap_or_else(|_| flags.cwd.clone());
    if flags.cont {
        if let Some(d) = overseer_core::session::most_recent(&root, Some(&cwd)) {
            return (d, true);
        }
        eprintln!(
            "overseer: no earlier session for {} — starting fresh",
            cwd.display()
        );
    }
    if flags.last {
        if let Some(d) = overseer_core::session::most_recent(&root, None) {
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
