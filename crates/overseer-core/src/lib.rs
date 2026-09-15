//! overseer-core — the agent engine.
//!
//! Phase 0 shape (playbook Ch.12 §12.2): canonical turn IR, append-only
//! event log, usage ledger, minimal tool set, Anthropic adapter, ReAct loop
//! with engine-enforced budgets. Frontends attach via overseer-proto.

pub mod agent;
pub mod compact;
pub mod event;
pub mod ir;
pub mod ledger;
pub mod perm;
pub mod profile;
pub mod provider;
pub mod stuck;
pub mod tools;
