//! `overseer consolidate` — the sleep-time memory pass.

use overseer_core::provider::Provider;

use crate::flags::parse_exec;
use crate::provider::build_provider;
use crate::session::{agent_config, apply_credentials};

/// `overseer consolidate [exec flags]` — sleep-time memory pass (P3.8):
/// small-tier dedupe of `<cwd>/memory/INDEX.md`, git-committed. Takes the
/// exec flag set (`--cwd`, `--provider`, `--model`, `--small-model`, …);
/// the aux call uses `--small-model` when given, else `--model`.
pub(crate) fn cmd_consolidate(args: &[String]) -> i32 {
    let mut flags = match parse_exec(args) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("overseer consolidate: {e}");
            return 2;
        }
    };
    flags.memory = true; // the command exists to touch memory
    let mut cred_cfg = agent_config(&flags);
    apply_credentials(&mut cred_cfg);
    let provider: std::sync::Arc<dyn Provider> = match build_provider(&flags, &cred_cfg.broker) {
        Ok(p) => p.into(),
        Err(msg) => {
            eprintln!("overseer: {msg}");
            return 2;
        }
    };
    let dir = flags.cwd.join("memory");
    let model = flags
        .small_model
        .clone()
        .unwrap_or_else(|| flags.model.clone());
    match overseer_core::memory::consolidate(provider.as_ref(), &model, &dir) {
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
