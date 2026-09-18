//! Keyword parser for inbound channel messages (P7-4).
//!
//! The keyword table is the *whole* verb set an inbound message may carry,
//! and none of the verbs execute anything (memo §D-P7: keywords never
//! direct-exec). `steer` and `queue` shape a drafted reply, `approve-only`
//! marks a message that may never run unsupervised — and every untrusted
//! event is forced to Notify-or-Draft by the daemon regardless of the rule
//! table, so the parser only classifies intent for routing and audit.

/// What the sender appears to want.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// `/steer <text>` — influence the thread's current/next turn.
    Steer,
    /// `/queue <text>` — hold for the thread's next turn.
    Queue,
    /// `/approve-only <text>` — approval required, never auto-execute.
    ApproveOnly,
    /// Ordinary message: no verb.
    Chat,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Intent::Steer => "steer",
            Intent::Queue => "queue",
            Intent::ApproveOnly => "approve_only",
            Intent::Chat => "chat",
        }
    }

    /// Event class suffix for triage routing (`msg.inbound.<verb>`).
    pub fn event_class(self) -> Option<&'static str> {
        match self {
            Intent::Steer => Some("msg.inbound.steer"),
            Intent::Queue => Some("msg.inbound.queue"),
            Intent::ApproveOnly => Some("msg.inbound.approve"),
            Intent::Chat => None,
        }
    }
}

/// The verb table. Left column is the accepted spelling (case-insensitive,
/// an optional leading `/` and a trailing `:` are tolerated).
const TABLE: &[(&str, Intent)] = &[
    ("steer", Intent::Steer),
    ("queue", Intent::Queue),
    ("approve", Intent::ApproveOnly),
    ("approve-only", Intent::ApproveOnly),
    ("approve_only", Intent::ApproveOnly),
];

/// Classify a message and return the body with the verb stripped.
pub fn parse(text: &str) -> (Intent, String) {
    let trimmed = text.trim_start();
    let (head, rest) = match trimmed.split_once(char::is_whitespace) {
        Some((h, r)) => (h, r.trim_start()),
        None => (trimmed, ""),
    };
    let verb = head
        .trim_start_matches('/')
        .trim_end_matches(':')
        .to_ascii_lowercase();
    match TABLE.iter().find(|(k, _)| *k == verb) {
        // A verb on its own still carries intent (an empty body is the
        // caller's business — the text is preserved verbatim).
        Some((_, intent)) => (*intent, rest.to_string()),
        None => (Intent::Chat, text.to_string()),
    }
}

/// Intent plus the event class for routing (bare `msg.inbound` for chat).
pub fn classify(text: &str) -> (Intent, String, &'static str) {
    let (intent, body) = parse(text);
    (intent, body, intent.event_class().unwrap_or("msg.inbound"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_table_maps_each_verb() {
        for (text, want, body) in [
            (
                "/steer focus on the failing test",
                Intent::Steer,
                "focus on the failing test",
            ),
            (
                "steer: focus on the failing test",
                Intent::Steer,
                "focus on the failing test",
            ),
            (
                "/queue later: bump the version",
                Intent::Queue,
                "later: bump the version",
            ),
            ("/approve-only ship it", Intent::ApproveOnly, "ship it"),
            ("/approve ship it", Intent::ApproveOnly, "ship it"),
            ("/APPROVE_ONLY ship it", Intent::ApproveOnly, "ship it"),
            ("/approve", Intent::ApproveOnly, ""),
            (
                "just a normal message",
                Intent::Chat,
                "just a normal message",
            ),
            ("/unknown verb", Intent::Chat, "/unknown verb"),
            ("", Intent::Chat, ""),
        ] {
            let (intent, got) = parse(text);
            assert_eq!(intent, want, "intent for {text:?}");
            assert_eq!(got, body, "body for {text:?}");
        }
        // Event classes stay namespaced and never collide with the base.
        assert_eq!(Intent::Chat.event_class(), None);
        assert_eq!(Intent::Steer.event_class(), Some("msg.inbound.steer"));
        assert_eq!(Intent::Queue.event_class(), Some("msg.inbound.queue"));
        assert_eq!(
            Intent::ApproveOnly.event_class(),
            Some("msg.inbound.approve")
        );
        let (intent, body, class) = classify("/queue hold this");
        assert_eq!(
            (intent, body.as_str(), class),
            (Intent::Queue, "hold this", "msg.inbound.queue")
        );
        let (_, _, class) = classify("hello");
        assert_eq!(class, "msg.inbound");
    }
}
