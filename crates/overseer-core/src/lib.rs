//! overseer-core — the agent engine.
//!
//! Phase 0 shape (playbook Ch.12 §12.2): canonical turn IR, append-only
//! event log, usage ledger, minimal tool set, Anthropic adapter, ReAct loop
//! with engine-enforced budgets. Frontends attach via overseer-proto.

pub mod agent;
pub mod backends;
pub mod compact;
pub mod computer_obs;
pub mod control;
pub mod cred;
pub mod event;
pub mod fuzzy;
pub mod harden;
pub mod hooks;
pub mod ir;
pub mod ledger;
pub mod manifest;
pub mod mcp;
pub mod mcp_config;
pub mod memory;
pub mod microagent;
pub mod modes;
pub mod onboard;
pub mod perm;
pub mod profile;
pub mod prompt;
pub mod provider;
pub mod recipe;
pub mod repomap;
pub mod rewind;
pub mod session;
pub mod skills;
pub mod stuck;
pub mod tokens;
pub mod tools;
pub mod toon;
/// P8-C tree-sitter registry: the grammar/query descriptors a real
/// `tree-sitter` binding registers against. Default-off and
/// dependency-free — see the module header for why the binding is not
/// linked (offline build + the zero-new-crate gate).
#[cfg(feature = "tree-sitter")]
pub mod tsitter;
