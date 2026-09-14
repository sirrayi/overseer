//! overseer-proto — wire protocol types (playbook Ch.2 §2.8, Invariant 5).
//!
//! One engine, many frontends: TUI, IDE, messaging, CI — all speak the same
//! NDJSON/JSON-RPC protocol to the app-server. Phase 0 keeps the transport
//! stub minimal: the event stream is `overseer_core::event::Event` serialized
//! as JSONL; the request side grows in Phase 2 (steering = protocol:
//! inject-now / queue / interrupt / fork).

use serde::{Deserialize, Serialize};

/// Client → engine requests (Phase 2 fills out steering + permissions).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Request {
    /// Start a new session.
    #[serde(rename = "session.start")]
    SessionStart { cwd: String, model: String },
    /// Resume an existing session directory.
    #[serde(rename = "session.resume")]
    SessionResume { dir: String },
    /// Submit a user turn.
    #[serde(rename = "turn.submit")]
    TurnSubmit { text: String },
    /// Interrupt the current turn (maps to SIGINT semantics of the tool set).
    #[serde(rename = "turn.interrupt")]
    TurnInterrupt,
}

/// Engine → client notifications beyond raw events (permissions, status).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "method", content = "params")]
pub enum Notification {
    /// Deterministic permission check needs a human verdict (L5).
    #[serde(rename = "permission.request")]
    PermissionRequest {
        request_id: String,
        tool: String,
        summary: String,
    },
    /// Permission decision from the frontend.
    #[serde(rename = "permission.response")]
    PermissionResponse { request_id: String, allowed: bool },
}
