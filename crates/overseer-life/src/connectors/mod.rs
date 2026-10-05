//! Connector registry. Each module ports one Synara providerUsage
//! fetcher (MIT, attribution in the file header + NOTICE) minus every
//! refresh path: stale OAuth tokens report `Expired`, never redeem.

pub mod claude;
pub mod codex;
pub mod cursor;
pub mod devin;
pub mod local_archives;
pub mod opencode;
pub mod x;

use crate::connector::Connector;

/// Every connector, in stable probe order. `local_archives` carries two
/// connectors (one per archive family).
pub fn all() -> Vec<Box<dyn Connector>> {
    vec![
        Box::new(claude::ClaudeConnector),
        Box::new(codex::CodexConnector),
        Box::new(cursor::CursorConnector),
        Box::new(devin::DevinConnector),
        Box::new(opencode::OpenCodeConnector),
        Box::new(local_archives::ClaudeArchiveConnector),
        Box::new(local_archives::CodexArchiveConnector),
        Box::new(x::XConnector::new()),
    ]
}
