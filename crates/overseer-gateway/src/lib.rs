//! overseer-gateway — the always-on daemon layer (playbook 12.7 §5.1–5.3).
//!
//! The daemon is deliberately thin and safe: it holds no secrets, runs no
//! dangerous tools, and never lets a model touch its own configuration
//! (the OpenClaw CVE-2026-45001 lesson — config mutation is not a
//! model-reachable surface; it changes only via the file or CLI).
//!
//! Pipeline: `trigger → dedup → triage → intervention gate → notify`.
//!   - triggers: cron schedules, file watchers, heartbeat, manual/webhook
//!   - triage: deterministic classify → ignore / notify / draft-for-review / act
//!   - gate: EV-of-interruption — push only when benefit − cost > θ,
//!     otherwise inbox; silence is a first-class action
//!   - notify: three tiers — silent (journal only) / inbox item / push
//!
//! The "act" tier spawns a sandboxed `overseer exec` run per task — the
//! daemon itself never executes agent work. Every open line between user
//! and agent is a durable inbox item: approve / reject / snooze writes a
//! receipt to the append-only journal.
//!
//! Messaging channels (P7-4) feed the same pipeline: verified inbound
//! messages arrive as `untrusted_source` events, which the daemon refuses
//! to act on (Notify-or-Draft only) and whose spawns carry the
//! `external=approve` autonomy floor.

pub mod channels;
pub mod config;
pub mod ctl;
pub mod daemon;
pub mod event;
pub mod gate;
pub mod inbox;
pub mod journal;
pub mod notify;
pub mod spawn;
pub mod triage;
pub mod trigger;
