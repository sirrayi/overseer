//! Notification tiers (playbook §2.4): silent → inbox → push.
//! "Never push what could be a digest line." Push v1 appends to
//! `push.jsonl` — frontends (TUI/desktop/messaging) tail it; a real
//! OS-notification channel lands with the desktop frontend.
//!
//! P7-5 adds the digest builder: open inbox items are collapsed into a few
//! cards (one per class, or a single overnight card while quiet hours run),
//! each expiring 24h after it was built — an unread item must not resurface
//! as a stale interruption. Thumbs feedback adjusts the benefit a class
//! carries into the intervention gate, so the digest learns what is noise.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

use crate::event::now_ms;
use crate::inbox::{InboxItem, ItemState};

#[derive(Debug, Serialize)]
struct PushRecord {
    ts_ms: u64,
    class: String,
    title: String,
    body: String,
}

pub struct PushQueue {
    path: PathBuf,
}

impl PushQueue {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Emit a push-tier notification. Returns false on write failure —
    /// the daemon degrades to inbox, never loses the item.
    pub fn push(&self, class: &str, title: &str, body: &str) -> bool {
        let rec = PushRecord {
            ts_ms: now_ms(),
            class: class.to_string(),
            title: title.to_string(),
            body: body.to_string(),
        };
        let Ok(mut line) = serde_json::to_string(&rec) else {
            return false;
        };
        line.push('\n');
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .is_ok()
    }
}
/// 24h: a card that was never looked at is stale, not pending.
pub const CARD_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// One digest card: several items collapsed into a single interruption.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Card {
    /// Stable within a digest build (`class` for class cards, `overnight`).
    pub id: String,
    pub class: String,
    pub title: String,
    /// One line per item, newest first, capped by [`MAX_CARD_LINES`].
    pub lines: Vec<String>,
    /// Items folded into this card (may exceed `lines.len()`).
    pub count: usize,
    pub created_ms: u64,
    pub expires_ms: u64,
    /// Benefit carried into the gate, after thumbs feedback.
    pub benefit: i16,
}

/// Cards stay glanceable: a digest that needs scrolling is a document.
pub const MAX_CARD_LINES: usize = 5;

/// Thumbs feedback for one class. `up` raises the benefit this class
/// carries (worth interrupting for), `down` decays it (noise).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Thumbs {
    pub up: u32,
    pub down: u32,
}

/// Benefit `base` (0..=100) adjusted by thumbs: +5 per net up, -10 per net
/// down, clamped to 0..=100. A down-weighted class decays faster than an up
/// can raise it — interruptions are guilty until proven useful.
pub fn benefit_with_thumbs(base: u8, thumbs: Thumbs) -> i16 {
    let net = thumbs.up as i32 - thumbs.down as i32;
    let delta = if net >= 0 { net * 5 } else { net * 10 };
    (base as i32 + delta).clamp(0, 100) as i16
}

/// Build the digest. While `quiet` is true every item folds into one
/// overnight card (it will be read at the morning breakpoint, not now);
/// otherwise items group by class, newest first. Silent/inbox-only.
pub fn build_digest(items: &[InboxItem], now: u64, quiet: bool) -> Vec<Card> {
    let open: Vec<&InboxItem> = items
        .iter()
        .filter(|i| matches!(i.state, ItemState::Open | ItemState::Snoozed))
        .collect();
    let mut cards: Vec<Card> = Vec::new();
    if open.is_empty() {
        return cards;
    }
    if quiet {
        cards.push(card_for(
            "overnight",
            "Overnight",
            &open,
            now,
            // Overnight cards are for the morning: the cheapest benefit.
            benefit_with_thumbs(30, Thumbs::default()),
        ));
        return cards;
    }
    // Snapshot the classes first: grouping needs deterministic order.
    let mut classes: Vec<String> = open.iter().map(|i| i.class.clone()).collect();
    classes.sort();
    classes.dedup();
    for class in classes {
        let group: Vec<&InboxItem> = open.iter().copied().filter(|i| i.class == class).collect();
        cards.push(card_for(
            &class,
            &class,
            &group,
            now,
            benefit_with_thumbs(50, Thumbs::default()),
        ));
    }
    cards.sort_by(|a, b| b.benefit.cmp(&a.benefit).then(a.class.cmp(&b.class)));
    cards
}

fn card_for(id: &str, title: &str, items: &[&InboxItem], now: u64, benefit: i16) -> Card {
    let mut sorted: Vec<&InboxItem> = items.to_vec();
    sorted.sort_by_key(|i| std::cmp::Reverse(i.created_ms));
    Card {
        id: id.to_string(),
        class: if id == "overnight" {
            "digest.overnight".to_string()
        } else {
            id.to_string()
        },
        title: title.to_string(),
        lines: sorted
            .iter()
            .take(MAX_CARD_LINES)
            .map(|i| format!("{} · {}", i.title, first_line(&i.body)))
            .collect(),
        count: sorted.len(),
        created_ms: now,
        expires_ms: now + CARD_TTL_MS,
        benefit,
    }
}

fn first_line(body: &str) -> String {
    let line = body.lines().next().unwrap_or("").trim();
    if line.chars().count() > 80 {
        line.chars().take(80).collect::<String>() + "…"
    } else {
        line.to_string()
    }
}

/// Drop expired cards. Keeps the digest a view of *now*, not of history —
/// the journal remains the record.
pub fn expire(cards: Vec<Card>, now: u64) -> Vec<Card> {
    cards.into_iter().filter(|c| c.expires_ms > now).collect()
}

/// Per-channel fan-out: one delivery per configured channel, duplicates
/// collapsed, unknown/empty channel names dropped. Rendering is plain text
/// — a channel never re-parses the digest.
pub fn fan_out(channels: &[String], card: &Card) -> Vec<(String, String)> {
    let mut seen: Vec<&str> = Vec::new();
    let mut out = Vec::new();
    for c in channels {
        let c = c.trim();
        if c.is_empty() || seen.contains(&c) {
            continue;
        }
        seen.push(c);
        out.push((c.to_string(), render_card(card)));
    }
    out
}

/// The text a channel delivers for one card.
pub fn render_card(card: &Card) -> String {
    let mut out = format!("{} ({} item(s))", card.title, card.count);
    for line in &card.lines {
        out.push_str("\n- ");
        out.push_str(line);
    }
    out
}

/// The digest as the frontends read it — a plain view over the ctl
/// protocol, no new engine (P7-6 reads the same shape).
pub fn digest_view(cards: &[Card]) -> serde_json::Value {
    serde_json::json!({
        "cards": cards,
        "count": cards.len(),
        "source": "inbox",
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("overseer-gateway-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn item(id: &str, class: &str, created: u64, body: &str) -> InboxItem {
        InboxItem {
            id: id.into(),
            created_ms: created,
            class: class.into(),
            source: "test".into(),
            title: format!("{class} · src"),
            body: body.into(),
            act_prompt: None,
            state: ItemState::Open,
            until_ms: None,
        }
    }

    #[test]
    fn digest_groups_by_class_and_expires_after_24h() {
        let now = 1_000_000u64;
        let items = vec![
            item("1", "note.low", now - 10, "first body"),
            item("2", "note.low", now - 20, "second body"),
            item("3", "ci.failed", now - 30, "build red"),
            InboxItem {
                state: ItemState::Rejected,
                ..item("4", "note.low", now - 5, "resolved already")
            },
        ];
        let cards = build_digest(&items, now, false);
        assert_eq!(cards.len(), 2, "one card per class, resolved items skipped");
        let note = cards.iter().find(|c| c.class == "note.low").unwrap();
        assert_eq!(note.count, 2);
        assert_eq!(note.lines.len(), 2);
        assert!(note.lines[0].contains("first body"), "newest first");
        assert_eq!(note.expires_ms, now + CARD_TTL_MS);
        // Expiry: a card built 24h ago is stale and drops out.
        assert_eq!(expire(cards.clone(), now + CARD_TTL_MS - 1).len(), 2);
        assert!(expire(cards.clone(), now + CARD_TTL_MS).is_empty());
        // The overnight card folds everything into one.
        let overnight = build_digest(&items, now, true);
        assert_eq!(overnight.len(), 1);
        assert_eq!(overnight[0].count, 3);
        assert_eq!(overnight[0].class, "digest.overnight");
        assert_eq!(overnight[0].benefit, 30, "cheaper than a daytime card");
        // Empty inbox → no cards, no error.
        assert!(build_digest(&[], now, false).is_empty());
        // Lines are capped so a card stays glanceable.
        let many: Vec<InboxItem> = (0..12)
            .map(|i| item(&format!("m{i}"), "noisy", now - i, "body"))
            .collect();
        let cards = build_digest(&many, now, false);
        assert_eq!(cards[0].count, 12);
        assert_eq!(cards[0].lines.len(), MAX_CARD_LINES);
        // The view is plain JSON over the same cards.
        let view = digest_view(&cards);
        assert_eq!(view["count"], 1);
        assert_eq!(view["source"], "inbox");
        assert!(view["cards"][0]["title"].as_str().is_some());
    }

    #[test]
    fn thumbs_decay_benefit_and_fan_out_renders_each_channel() {
        // A thumbs-down decays twice as fast as a thumbs-up raises.
        assert_eq!(benefit_with_thumbs(50, Thumbs::default()), 50);
        assert_eq!(benefit_with_thumbs(50, Thumbs { up: 2, down: 0 }), 60);
        assert_eq!(benefit_with_thumbs(50, Thumbs { up: 0, down: 2 }), 30);
        assert_eq!(benefit_with_thumbs(50, Thumbs { up: 1, down: 1 }), 50);
        // Clamped at both ends.
        assert_eq!(benefit_with_thumbs(5, Thumbs { up: 0, down: 5 }), 0);
        assert_eq!(benefit_with_thumbs(95, Thumbs { up: 5, down: 0 }), 100);

        let card = Card {
            id: "ci.failed".into(),
            class: "ci.failed".into(),
            title: "ci.failed".into(),
            lines: vec!["build red".into()],
            count: 1,
            created_ms: 0,
            expires_ms: CARD_TTL_MS,
            benefit: 50,
        };
        let channels = vec![
            "telegram".to_string(),
            " telegram ".to_string(),
            "desktop".to_string(),
            String::new(),
        ];
        let deliveries = fan_out(&channels, &card);
        assert_eq!(deliveries.len(), 2, "duplicates and blanks collapse");
        assert_eq!(deliveries[0].0, "telegram");
        assert_eq!(deliveries[1].0, "desktop");
        assert!(deliveries[0].1.contains("ci.failed (1 item(s))"));
        assert!(deliveries[0].1.contains("- build red"));
        assert!(fan_out(&[], &card).is_empty());
    }

    #[test]
    fn push_appends_json_lines() {
        let dir = tmpdir("notify-push");
        let path = dir.join("push.jsonl");
        let _ = std::fs::remove_file(&path);
        let queue = PushQueue::new(path.clone());
        assert!(queue.push("inbox", "hello", "world"));
        assert!(queue.push("inbox", "second", "body2"));
        let text = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["class"], "inbox");
        assert_eq!(first["title"], "hello");
        assert_eq!(first["body"], "world");
        let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second["title"], "second");
        assert_eq!(second["body"], "body2");
    }
}
