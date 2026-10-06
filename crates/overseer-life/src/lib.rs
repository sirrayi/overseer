//! Overseer Life: personal-data connectors (Phase 0, backend only).
//!
//! Every connector is read-only, synchronous and injectable: home dir,
//! env, clock, HTTP and process spawning all come from `Ctx`, so tests
//! never touch the real home directory, keychain or network. Fetches
//! answer a `Snapshot` whose `status` carries every outcome — nothing
//! here panics on bad input, and secrets never reach a log, an error
//! string or serialized output (only 18-char sha256-base64url
//! fingerprints).
//!
//! Hard rules baked in:
//! - no OAuth token/refresh endpoint calls for other apps' credentials —
//!   an expired stored token reports `Status::Expired` and stops (X's own
//!   tokens are the exception: they belong to this app);
//! - credential files and other apps' state are opened read-only
//!   (Cursor's state.vscdb via `sqlite3 -readonly`);
//! - keychain secret reads require `ctx.allow_keychain_secrets` — the
//!   probe keeps it off;
//! - the HTTP layer caps response size, enforces a per-call origin
//!   allow-list and supports a per-run request budget so the live probe
//!   can never exceed one request per source.

pub mod connector;
pub mod connectors;
pub mod creds;
pub mod http;
pub mod snapshot;
pub mod time;

pub use connector::{Connector, ConnectorInfo, Ctx, Discovered, Platform};
pub use snapshot::{
    AccountRef, ErrorKind, Metric, Provenance, ProvenanceKind, Snapshot, SourceId, Status,
    UsageLimit, UsageLine,
};

/// All connectors in probe order.
pub fn registry() -> Vec<Box<dyn Connector>> {
    connectors::all()
}
