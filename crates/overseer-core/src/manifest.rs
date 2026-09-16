//! Run manifest (playbook Ch.8 §7.3.7, Ch.12 §4.5): the provenance record
//! the public reporting standard requires of every published number —
//! harness identity + commit, verbatim system-prompt hash, tool inventory,
//! model + effort, budgets, policy posture.
//!
//! Written once at session creation into `<session-dir>/`:
//! - `manifest.json` — the reporting-standard field set
//! - `system_prompt.txt` — verbatim audit artifact the hash covers
//! - `tools.json` — full tool-spec dump (name + description + schema),
//!   the input for rug-pull hash-checking
//!
//! This is run metadata, not session state — `events.jsonl` stays the sole
//! conversation source (Invariant 1). `resume` never rewrites it: the
//! manifest describes the session's creation configuration. Forked
//! sessions copy the parent's event log only, so they carry no manifest —
//! eval runs are always fresh sessions.

use std::path::Path;

use sha2::{Digest, Sha256};

use crate::agent::AgentConfig;
use crate::perm::Preset;
use crate::provider::Provider;
use crate::tools::ToolRegistry;

/// Canonical prompt form: segment texts joined by "\n\n". Providers may
/// encode them as discrete blocks on the wire — this canonical join is
/// what `system_prompt.sha256` covers, stable across wire encodings.
const SEGMENT_JOIN: &str = "\n\n";

/// Emit the three manifest artifacts into a freshly created session dir.
/// Called once from `Agent::start`; the dir is already proven writable
/// (events.jsonl + ledger.jsonl were created first).
pub fn write(
    session_dir: &Path,
    session_id: &str,
    config: &AgentConfig,
    provider: &dyn Provider,
    tools: &ToolRegistry,
) -> std::io::Result<()> {
    // The same assembler the ReAct loop calls each turn — captured here at
    // session start, so the hash identifies the initial system prompt.
    // Memory-index or skill changes later in the run alter subsequent
    // prompts; `scope: session_start` records that honestly.
    let system = crate::prompt::assemble(config);
    let prompt_text = system
        .iter()
        .map(|s| s.text.as_str())
        .collect::<Vec<_>>()
        .join(SEGMENT_JOIN);

    let tools_json = serde_json::to_string_pretty(
        &tools
            .specs
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect::<Vec<_>>(),
    )
    .map_err(std::io::Error::other)?;

    let manifest = serde_json::json!({
        "schema": "overseer.run-manifest/1",
        "harness": {
            "name": "overseer",
            "version": env!("CARGO_PKG_VERSION"),
            "commit": option_env!("OVERSEER_GIT_SHA").unwrap_or("unknown"),
        },
        "session": {
            "id": session_id,
            "created_ms": crate::event::now_ms(),
            "cwd": config.cwd.display().to_string(),
        },
        "model": {
            "name": config.model,
            "provider": provider.name(),
            // Overseer never sets temperature — adapters send none, so the
            // provider default applies. Recorded as a policy, not a number.
            "temperature": "provider_default",
            "effort": config.effort.map(|e| e.as_str()),
            "thinking_budget": config.thinking_budget,
            "small_model": config.small_model,
            // "profiled" = real price table; "estimated" = FALLBACK mid-tier
            // guess — reports must not quote estimated cost as measured.
            "cost_basis": if crate::profile::known(&config.model) {
                "profiled"
            } else {
                "estimated"
            },
        },
        "limits": {
            "max_steps": config.max_steps,
            "max_cost_usd": config.max_cost_usd,
            "max_output_tokens": config.max_output_tokens,
        },
        "policy": {
            "preset": preset_name(config.policy_preset),
            "full_access": config.full_access,
            "sandbox_bash": config.sandbox_bash,
            "ask_channel": if config.ask_handler.is_some() { "human" } else { "headless" },
            "disabled_tools": config.disabled_tools,
        },
        "context": {
            "auto_compact": config.auto_compact,
            "compact_at": config.compact_at,
            "keep_tool_results": config.keep_tool_results,
            "verify_cmd": config.verify_cmd,
            "verify_block_cap": config.verify_block_cap,
        },
        "memory": { "enabled": config.memory_dir.is_some() },
        "system_prompt": {
            "sha256": hex_sha256(prompt_text.as_bytes()),
            "bytes": prompt_text.len(),
            "scope": "session_start",
            "file": "system_prompt.txt",
        },
        "tools": {
            "sha256": hex_sha256(tools_json.as_bytes()),
            "count": tools.specs.len(),
            "names": tools.specs.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
            "file": "tools.json",
        },
    });

    std::fs::write(session_dir.join("system_prompt.txt"), prompt_text)?;
    std::fs::write(session_dir.join("tools.json"), tools_json)?;
    std::fs::write(
        session_dir.join("manifest.json"),
        serde_json::to_string_pretty(&manifest).map_err(std::io::Error::other)?,
    )
}

fn preset_name(p: Preset) -> &'static str {
    match p {
        Preset::ReadOnly => "read_only",
        Preset::WorkspaceWrite => "workspace_write",
        Preset::Plan => "plan",
    }
}

fn hex_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{ProviderError, Request, Response};
    use std::path::PathBuf;

    struct Stub;
    impl Provider for Stub {
        fn complete(&self, _: &Request) -> Result<Response, ProviderError> {
            unreachable!()
        }
        fn name(&self) -> &'static str {
            "stub"
        }
    }

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-manifest-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn sha256_hex_matches_known_vector() {
        assert_eq!(
            hex_sha256(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn writes_three_artifacts_with_reporting_fields() {
        let dir = tmpdir();
        let cfg = AgentConfig::default();
        let tools = ToolRegistry::readonly(crate::perm::Policy::allow_all());
        write(&dir, "s1", &cfg, &Stub, &tools).unwrap();

        let prompt = std::fs::read_to_string(dir.join("system_prompt.txt")).unwrap();
        let tools_json: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("tools.json")).unwrap())
                .unwrap();
        let m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("manifest.json")).unwrap())
                .unwrap();

        assert_eq!(m["schema"], "overseer.run-manifest/1");
        assert_eq!(m["harness"]["name"], "overseer");
        assert_eq!(m["session"]["id"], "s1");
        assert_eq!(m["model"]["provider"], "stub");
        // The hash covers the verbatim artifact byte-for-byte.
        assert_eq!(
            m["system_prompt"]["sha256"].as_str().unwrap(),
            hex_sha256(prompt.as_bytes())
        );
        assert_eq!(
            m["system_prompt"]["bytes"].as_u64().unwrap() as usize,
            prompt.len()
        );
        // Tool inventory is complete and hash-bound to the spec dump.
        let names: Vec<&str> = tools.specs.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(m["tools"]["count"].as_u64().unwrap() as usize, names.len());
        for n in &names {
            assert!(m["tools"]["names"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == n));
        }
        assert_eq!(
            m["tools"]["sha256"].as_str().unwrap(),
            hex_sha256(
                std::fs::read_to_string(dir.join("tools.json"))
                    .unwrap()
                    .as_bytes()
            )
        );
        // tools.json carries the full specs (rug-pull hash-check input).
        assert_eq!(tools_json.as_array().unwrap().len(), names.len());
        assert!(tools_json[0]["input_schema"].is_object());
    }

    #[test]
    fn hashes_are_deterministic() {
        let a = tmpdir();
        let b = tmpdir();
        let cfg = AgentConfig::default();
        let tools = ToolRegistry::readonly(crate::perm::Policy::allow_all());
        write(&a, "s1", &cfg, &Stub, &tools).unwrap();
        write(&b, "s2", &cfg, &Stub, &tools).unwrap();
        let ma: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(a.join("manifest.json")).unwrap())
                .unwrap();
        let mb: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(b.join("manifest.json")).unwrap())
                .unwrap();
        assert_eq!(ma["system_prompt"]["sha256"], mb["system_prompt"]["sha256"]);
        assert_eq!(ma["tools"]["sha256"], mb["tools"]["sha256"]);
    }
}
