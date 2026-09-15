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
    /// Steer mid-run: injected as a user message at the next tool-launch
    /// boundary — skipped calls get synthetic results (P2.4).
    #[serde(rename = "turn.steer")]
    TurnSteer { text: String },
    /// Queue a message for delivery when the run goes idle (queued ≠ sent).
    #[serde(rename = "turn.queue")]
    TurnQueue { text: String },
    /// Cancel one queued message by index (queue-strip per-item cancel).
    #[serde(rename = "turn.queue_cancel")]
    TurnQueueCancel { index: usize },
    /// Branch the session at an event boundary into a new session dir.
    #[serde(rename = "session.fork")]
    SessionFork {
        /// Fork point; `None` = current head.
        at_event: Option<u64>,
    },
    /// Swap the permission preset (mode badge): workspace | readonly | plan.
    #[serde(rename = "session.set_preset")]
    SessionSetPreset { preset: String },
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
