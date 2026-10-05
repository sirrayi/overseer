//! `overseer consolidate` — the sleep-time memory pass.

use overseer_core::provider::Provider;

use crate::flags::parse_exec;
use crate::provider::build_provider;
use crate::session::{agent_config, apply_credentials, memory_store_list};

/// `overseer consolidate [exec flags]` — sleep-time memory pass (P3.8) over
/// the resolved stores ([`memory_store_list`]): per store, small-tier
/// dedupe of `INDEX.md` and use-journal compaction; then distillation of
/// the project store's new episodes; git-committed. Takes the
/// exec flag set (`--cwd`, `--provider`, `--model`, `--small-model`, …);
/// the aux call uses `--small-model` when given, else `--model`.
pub(crate) fn cmd_consolidate(args: &[String]) -> i32 {
    let flags = match parse_exec(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer consolidate: {e}");
            return 2;
        }
    };
    if flags.bare || flags.no_memory {
        eprintln!("overseer consolidate: memory is off (--bare / --no-memory)");
        return 2;
    }
    let mut cred_cfg = agent_config(&flags);
    apply_credentials(&mut cred_cfg);
    let provider: std::sync::Arc<dyn Provider> = match build_provider(&flags, &cred_cfg.broker) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer: {msg}");
            return 2;
        }
    };
    let stores: Vec<_> = memory_store_list(&flags, &cred_cfg.cwd)
        .into_iter()
        .filter(|(_, d)| d.is_dir())
        .collect();
    if stores.is_empty() {
        eprintln!("overseer consolidate: no memory store yet (see `overseer memory where`)");
        return 1;
    }
    let model = flags
        .small_model
        .clone()
        .unwrap_or_else(|| flags.model.clone());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    match overseer_core::memory::consolidate_stores(provider.as_ref(), &model, &stores, now) {
        Ok(msg) => {
            println!("{msg}");
            0
        }
        Err(e) => {
            eprintln!("overseer consolidate: {e}");
            1
        }
    }
}
