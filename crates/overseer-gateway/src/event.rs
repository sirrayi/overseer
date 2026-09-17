//! Trigger events — the unit that flows through the pipeline.

use serde::{Deserialize, Serialize};

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// A normalized event produced by any trigger source. `class` is the
/// routing key for triage rules ("git.dirty", "heartbeat", "ci.failed").
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriggerEvent {
    pub id: String,
    /// Trigger spec id ("nightly-review", "watch-inbox", manual id…).
    pub source: String,
    /// Dot-namespaced event class for triage routing.
    pub class: String,
    /// Free-form payload (diff summary, webhook body, heartbeat note).
    pub payload: String,
    pub fired_at_ms: u64,
    /// P7-4: the content came from an untrusted source (a channel user, not
    /// the operator). Serde-defaulted so pre-existing events and configs
    /// round-trip unchanged; the daemon refuses to act on such an event.
    #[serde(default)]
    pub untrusted_source: bool,
    /// P7-4: channel envelope for thread routing. `None` for every local
    /// trigger — additive, so old records still parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<ChannelOrigin>,
}

/// Who sent an inbound channel message, and where it belongs. A channel
/// identity is a claim, not a verification — it is audit metadata and a
/// routing key, never an authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelOrigin {
    pub channel: String,
    pub sender: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    /// Keyword intent (`chat`/`steer`/`queue`/`approve_only`), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub intent: Option<String>,
}

impl TriggerEvent {
    pub fn new(
        source: impl Into<String>,
        class: impl Into<String>,
        payload: impl Into<String>,
    ) -> Self {
        TriggerEvent {
            id: uuid::Uuid::now_v7().to_string(),
            source: source.into(),
            class: class.into(),
            payload: payload.into(),
            fired_at_ms: now_ms(),
            untrusted_source: false,
            origin: None,
        }
    }

    /// An inbound channel message: `source = <channel>:<sender>`,
    /// `class = msg.inbound`, always untrusted (P7-4).
    pub fn from_channel(channel: &str, sender: &str, thread: Option<&str>, text: &str) -> Self {
        let mut ev = TriggerEvent::new(format!("{channel}:{sender}"), "msg.inbound", text);
        ev.untrusted_source = true;
        ev.origin = Some(ChannelOrigin {
            channel: channel.to_string(),
            sender: sender.to_string(),
            thread: thread.map(str::to_string),
            intent: None,
        });
        ev
    }

    /// Dedup identity: same source+class+payload within the window is a
    /// duplicate — one inbox item per real-world happening, not per poll.
    pub fn dedup_key(&self) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.source.hash(&mut h);
        self.class.hash(&mut h);
        self.payload.hash(&mut h);
        format!("{:016x}", h.finish())
    }
}

/// Rolling dedup table — key → first-seen timestamp. Entries expire out
/// of the window; bounded by trigger rate so a linear sweep is fine.
#[derive(Debug, Default)]
pub struct Dedup {
    seen: Vec<(String, u64)>,
    window_ms: u64,
}

impl Dedup {
    pub fn new(window_s: u64) -> Self {
        Self {
            seen: Vec::new(),
            window_ms: window_s * 1000,
        }
    }

    /// True if this event is new (and records it); false if a repeat.
    pub fn admit(&mut self, ev: &TriggerEvent) -> bool {
        let now = now_ms();
        self.seen
            .retain(|(_, t)| now.saturating_sub(*t) < self.window_ms);
        let key = ev.dedup_key();
        if self.seen.iter().any(|(k, _)| k == &key) {
            return false;
        }
        self.seen.push((key, ev.fired_at_ms));
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_sets_source_class_payload_and_timestamp() {
        let ev = TriggerEvent::new("nightly-review", "git.dirty", "diff summary");
        assert_eq!(ev.source, "nightly-review");
        assert_eq!(ev.class, "git.dirty");
        assert_eq!(ev.payload, "diff summary");
        assert!(!ev.id.is_empty());
        assert!(ev.fired_at_ms > 0);
    }

    #[test]
    fn dedup_key_stable_for_identical_fields() {
        let a = TriggerEvent::new("watch-inbox", "heartbeat", "note");
        let b = TriggerEvent::new("watch-inbox", "heartbeat", "note");
        // id and fired_at_ms differ but must not affect the key.
        assert_ne!(a.id, b.id);
        assert_eq!(a.dedup_key(), b.dedup_key());
    }

    #[test]
    fn dedup_key_differs_on_any_field_change() {
        let base = TriggerEvent::new("src", "git.dirty", "payload");
        let base_key = base.dedup_key();
        let other_source = TriggerEvent::new("other", "git.dirty", "payload");
        let other_class = TriggerEvent::new("src", "ci.failed", "payload");
        let other_payload = TriggerEvent::new("src", "git.dirty", "changed");
        assert_ne!(base_key, other_source.dedup_key());
        assert_ne!(base_key, other_class.dedup_key());
        assert_ne!(base_key, other_payload.dedup_key());
    }

    #[test]
    fn admit_first_then_drop_immediate_repeat() {
        let mut d = Dedup::new(60);
        let ev = TriggerEvent::new("watch-inbox", "heartbeat", "note");
        assert!(d.admit(&ev));
        assert!(!d.admit(&ev));
    }

    #[test]
    fn readmit_after_window_expiry_with_zero_window() {
        let mut d = Dedup::new(0);
        let ev = TriggerEvent::new("watch-inbox", "heartbeat", "note");
        assert!(d.admit(&ev));
        // Zero window expires immediately: same key re-admits without sleeping.
        assert!(d.admit(&ev));
    }

    #[test]
    fn expiry_prunes_old_entries_so_memory_bounded() {
        let mut d = Dedup::new(60);
        let mut old = TriggerEvent::new("watch-inbox", "heartbeat", "note");
        old.fired_at_ms = now_ms().saturating_sub(61_000);
        assert!(d.admit(&old));
        assert_eq!(d.seen.len(), 1);
        let fresh = TriggerEvent::new("watch-inbox", "heartbeat", "note");
        assert!(d.admit(&fresh));
        // Old entry was pruned instead of accumulating: still one entry.
        assert_eq!(d.seen.len(), 1);
    }
}
