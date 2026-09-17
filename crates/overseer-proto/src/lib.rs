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
    /// Submit a user turn. `sender`/`thread` are optional (P7-5) so a
    /// channel frontend can name the conversation a turn belongs to; both
    /// are serde-defaulted, so pre-P7 clients keep parsing unchanged.
    #[serde(rename = "turn.submit")]
    TurnSubmit {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
    },
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
    /// Queue an outbound channel message (P7-5). The engine drafts it; a
    /// human approves it before anything leaves the machine, so this method
    /// is never a "send" in the transport sense.
    #[serde(rename = "channel.send")]
    ChannelSend {
        to: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread: Option<String>,
        text: String,
    },
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Wire contract: every method string, verbatim. These are the messages
    /// frontends and channels speak — renaming one is a breaking change that
    /// must be deliberate, and this test is the alarm (R1-F10).
    #[test]
    fn every_request_tag_is_pinned() {
        let cases: Vec<(Request, &str)> = vec![
            (
                Request::SessionStart {
                    cwd: "/tmp".into(),
                    model: "m".into(),
                },
                "session.start",
            ),
            (
                Request::SessionResume { dir: "/tmp".into() },
                "session.resume",
            ),
            (
                Request::TurnSubmit {
                    text: "hi".into(),
                    sender: None,
                    thread: None,
                },
                "turn.submit",
            ),
            (Request::TurnInterrupt, "turn.interrupt"),
            (Request::TurnSteer { text: "go".into() }, "turn.steer"),
            (Request::TurnQueue { text: "later".into() }, "turn.queue"),
            (Request::TurnQueueCancel { index: 2 }, "turn.queue_cancel"),
            (Request::SessionFork { at_event: None }, "session.fork"),
            (
                Request::SessionSetPreset {
                    preset: "plan".into(),
                },
                "session.set_preset",
            ),
            (
                Request::ChannelSend {
                    to: "ops".into(),
                    thread: Some("t1".into()),
                    text: "hello".into(),
                },
                "channel.send",
            ),
        ];
        for (req, tag) in cases {
            let v = serde_json::to_value(&req).unwrap();
            assert_eq!(v["method"], tag, "tag for {req:?}");
            let back: Request = serde_json::from_str(&v.to_string()).unwrap();
            assert_eq!(serde_json::to_value(&back).unwrap(), v);
        }
    }

    #[test]
    fn turn_submit_stays_wire_compatible() {
        // A pre-P7 client sends only `text` — it must still parse.
        let old: Request = serde_json::from_str(r#"{"method":"turn.submit","params":{"text":"hi"}}"#)
            .expect("old shape parses");
        match old {
            Request::TurnSubmit {
                text,
                sender,
                thread,
            } => {
                assert_eq!(text, "hi");
                assert!(sender.is_none() && thread.is_none());
            }
            other => panic!("expected TurnSubmit, got {other:?}"),
        }
        // With a channel envelope, both travel.
        let new: Request = serde_json::from_str(
            r#"{"method":"turn.submit","params":{"text":"hi","sender":"77","thread":"-1001"}}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&new).unwrap()["params"]["thread"],
            "-1001"
        );
        // An absent optional field is not serialized back as null.
        let bare = serde_json::to_value(Request::TurnSubmit {
            text: "x".into(),
            sender: None,
            thread: None,
        })
        .unwrap();
        assert!(bare["params"].get("thread").is_none());
        assert!(bare["params"].get("sender").is_none());
    }

    #[test]
    fn notification_tags_are_pinned() {
        let cases: Vec<(Notification, &str)> = vec![
            (
                Notification::PermissionRequest {
                    request_id: "r".into(),
                    tool: "bash".into(),
                    summary: "rm -rf".into(),
                },
                "permission.request",
            ),
            (
                Notification::PermissionResponse {
                    request_id: "r".into(),
                    allowed: false,
                },
                "permission.response",
            ),
        ];
        for (n, tag) in cases {
            let v = serde_json::to_value(&n).unwrap();
            assert_eq!(v["method"], tag);
            let back: Notification = serde_json::from_str(&v.to_string()).unwrap();
            assert_eq!(serde_json::to_value(&back).unwrap(), v);
        }
    }
}
