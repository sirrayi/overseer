//! Messaging channels (P7-4): one channel first, not a matrix.
//!
//! Inbound path: `verify → allowlist → rate limit → parse → TriggerEvent`.
//! Every channel-sourced event is marked `untrusted_source`, so the daemon
//! forces it to Notify-or-Draft (an `Act` rule is downgraded) and any spawn
//! it causes carries the `external=approve` autonomy floor plus the
//! `OVERSEER_UNTRUSTED_SOURCE` marker the engine arms on.
//!
//! Outbound path: drafted here, sent by the outbox (P7-5) after approval —
//! the ladder's `external → Ask` is never bypassed by a channel.
//!
//! Nothing in this module executes agent work: keywords classify intent,
//! threads pick a session directory, and the daemon owns the pipeline.

pub mod keywords;
pub mod sentinel;
pub mod telegram;
pub mod threads;
pub mod webhook;

use crate::event::{ChannelOrigin, TriggerEvent};

/// The marker an untrusted-originated spawn exports. Literal on purpose:
/// it is the frozen core contract (`overseer_core::agent::UNTRUSTED_ENV`)
/// and this crate must not depend on `overseer-core` to name it.
pub const UNTRUSTED_ENV: &str = "OVERSEER_UNTRUSTED_SOURCE";

/// Arguments appended to every spawn caused by an untrusted event: external
/// effects stay at approval no matter what the local config says.
pub const UNTRUSTED_AUTONOMY_FLOOR: [&str; 2] = ["--autonomy", "external=approve"];

/// The value the spawn exports for a channel origin.
pub fn untrusted_marker(origin: &ChannelOrigin) -> String {
    format!("channel:{}:{}", origin.channel, origin.sender)
}

/// Channel envelope → trigger event. The class stays `msg.inbound` for
/// ordinary chat and gains a verb suffix (`msg.inbound.steer`,
/// `msg.inbound.queue`, `msg.inbound.approve`) for keyword messages, so
/// operators can route verbs separately — the verb never changes what the
/// daemon may *do*, only how the message is filed.
pub fn event_for(inbound: &webhook::Inbound) -> TriggerEvent {
    let (intent, body, class) = keywords::classify(&inbound.text);
    let mut ev = TriggerEvent::from_channel(
        &inbound.channel,
        &inbound.sender,
        inbound.thread.as_deref(),
        &body,
    );
    ev.class = class.to_string();
    ev.payload = body;
    // The intent is audit metadata, not an action: keep it visible.
    ev.origin = Some(ChannelOrigin {
        channel: inbound.channel.clone(),
        sender: inbound.sender.clone(),
        thread: inbound.thread.clone(),
        intent: Some(intent.as_str().to_string()),
    });
    ev
}

/// A refusal from an ingress (bad signature, disallowed sender, rate
/// limit). Surfaced as an event rather than a silent drop: the daemon
/// journals it, and `untrusted_source` keeps it inside the same
/// Notify-or-Draft floor as the message it refused.
pub fn rejection_event(source: &str, reason: &str) -> TriggerEvent {
    let mut ev = TriggerEvent::new(source, "channel.rejected", reason);
    ev.untrusted_source = true;
    ev
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_events_are_untrusted_and_typed() {
        let chat = webhook::Inbound {
            channel: "telegram".into(),
            sender: "77".into(),
            thread: Some("-1001".into()),
            text: "hello".into(),
        };
        let ev = event_for(&chat);
        assert!(
            ev.untrusted_source,
            "channel content is never operator input"
        );
        assert_eq!(ev.class, "msg.inbound");
        assert_eq!(ev.source, "telegram:77");
        assert_eq!(ev.payload, "hello");
        let origin = ev.origin.clone().unwrap();
        assert_eq!(untrusted_marker(&origin), "channel:telegram:77");
        assert_eq!(origin.intent.as_deref(), Some("chat"));

        // Keywords are classed, never acted on.
        let steer = webhook::Inbound {
            text: "/steer look at the failing test".into(),
            ..chat.clone()
        };
        let ev = event_for(&steer);
        assert_eq!(ev.class, "msg.inbound.steer");
        assert_eq!(ev.payload, "look at the failing test");
        assert!(ev.untrusted_source);
        assert_eq!(ev.origin.as_ref().unwrap().intent.as_deref(), Some("steer"));

        // The floor is a fixed pair of argv tokens and a literal marker.
        assert_eq!(UNTRUSTED_AUTONOMY_FLOOR, ["--autonomy", "external=approve"]);
        assert_eq!(UNTRUSTED_ENV, "OVERSEER_UNTRUSTED_SOURCE");

        let rej = rejection_event("telegram:77", "webhook: signature rejected");
        assert_eq!(rej.class, "channel.rejected");
        assert!(rej.untrusted_source);
    }
}
