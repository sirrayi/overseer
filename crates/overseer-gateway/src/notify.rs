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

/// The inbox as a frontend reads it: the same field set `ctl.inbox_list`
/// has always served, built in one place so the desktop view and the
/// existing CLI cannot drift apart (P7-6: a view over the protocol, not a
/// second engine).
pub fn inbox_view(items: &[InboxItem]) -> serde_json::Value {
    let items: Vec<serde_json::Value> = items
        .iter()
        .map(|i| {
            serde_json::json!({
                "id": i.id, "state": i.state, "class": i.class,
                "source": i.source, "title": i.title, "body": i.body,
                "act_prompt": i.act_prompt, "created_ms": i.created_ms,
                "until_ms": i.until_ms,
            })
        })
        .collect();
    serde_json::json!({ "items": items, "count": items.len() })
}

// ── P7-6: the desktop notifier ────────────────────────────────────────

/// What a desktop notification offers. These are the three verbs the inbox
/// already understands — the notifier invents no new state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyAction {
    Approve,
    Reject,
    Snooze,
}

impl NotifyAction {
    /// The button label a desktop backend renders.
    pub fn label(self) -> &'static str {
        match self {
            NotifyAction::Approve => "Approve",
            NotifyAction::Reject => "Reject",
            NotifyAction::Snooze => "Snooze",
        }
    }
}

/// Default snooze window when a notification's Snooze button is pressed.
pub const DEFAULT_SNOOZE_MS: u64 = 3_600_000;

/// The control-plane request a button maps to. The notifier never mutates
/// daemon state directly: pressing Approve sends exactly what the CLI's
/// `inbox act` sends, so both surfaces stay one protocol.
pub fn action_request(action: NotifyAction, item_id: &str) -> crate::ctl::CtlRequest {
    match action {
        NotifyAction::Approve => crate::ctl::CtlRequest::InboxAct {
            id: item_id.to_string(),
        },
        NotifyAction::Reject => crate::ctl::CtlRequest::InboxDecide {
            id: item_id.to_string(),
            decision: "reject".to_string(),
            snooze_ms: None,
        },
        NotifyAction::Snooze => crate::ctl::CtlRequest::InboxDecide {
            id: item_id.to_string(),
            decision: "snooze".to_string(),
            snooze_ms: Some(DEFAULT_SNOOZE_MS),
        },
    }
}

/// A desktop notification backend. `notify` is expected to be
/// fire-and-forget: a failed notification is logged, never queued forever.
pub trait Notifier {
    fn notify(&self, card: &Card, actions: &[NotifyAction]) -> Result<(), String>;
    /// Backend name for the journal ("log", "osascript", "notify-send"…).
    fn backend(&self) -> &'static str;
}

/// The headless/test backend: appends one JSON line per notification to the
/// daemon's notify log. This is also the fallback on every platform whose
/// native helper is missing — an invisible notification is still recorded.
pub struct LogNotifier {
    path: PathBuf,
}

impl LogNotifier {
    pub fn new(path: PathBuf) -> Self {
        LogNotifier { path }
    }
}

impl Notifier for LogNotifier {
    fn notify(&self, card: &Card, actions: &[NotifyAction]) -> Result<(), String> {
        use std::io::Write;
        let rec = serde_json::json!({
            "ts_ms": now_ms(),
            "card": card.id,
            "title": card.title,
            "body": render_card(card),
            "actions": actions.iter().map(|a| a.label()).collect::<Vec<_>>(),
        });
        let mut line = serde_json::to_string(&rec).map_err(|e| e.to_string())?;
        line.push('\n');
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .and_then(|mut f| f.write_all(line.as_bytes()))
            .map_err(|e| format!("notify: {}: {e}", self.path.display()))
    }

    fn backend(&self) -> &'static str {
        "log"
    }
}

/// The per-OS CLI backend: macOS `osascript`, Linux `notify-send`. Chosen
/// by `cfg!` at build time and probed at runtime —
/// when the helper is missing the caller falls back to [`LogNotifier`].
///
/// The native *framework* shells (UNUserNotificationCenter, libnotify
/// bindings) are deferred: they need a platform dependency
/// this phase does not take, and the CLI path already produces a real
/// notification with real Approve/Reject/Snooze targets.
pub struct CliNotifier {
    program: PathBuf,
    log_fallback: LogNotifier,
}

impl CliNotifier {
    /// The helper this build would use, if it exists on the system.
    pub fn detect(log_path: PathBuf) -> Option<Self> {
        let candidates: &[&str] = if cfg!(target_os = "macos") {
            &["/usr/bin/osascript"]
        } else if cfg!(target_os = "linux") {
            &["/usr/bin/notify-send", "/bin/notify-send"]
        } else {
            &[]
        };
        let program = candidates.iter().map(PathBuf::from).find(|p| p.exists())?;
        Some(CliNotifier {
            program,
            log_fallback: LogNotifier::new(log_path),
        })
    }
}

impl Notifier for CliNotifier {
    fn notify(&self, card: &Card, actions: &[NotifyAction]) -> Result<(), String> {
        // The notification is always recorded (audit), then shown.
        let logged = self.log_fallback.notify(card, actions);
        let text = format!("{} — {} item(s)", card.title, card.count);
        let action_hint = actions
            .iter()
            .map(|a| a.label())
            .collect::<Vec<_>>()
            .join(" / ");
        let script = if cfg!(target_os = "macos") {
            format!(
                "display notification {:?} with title {:?}",
                text, action_hint
            )
        } else {
            format!("{action_hint}\n{text}")
        };
        let args: Vec<String> = if cfg!(target_os = "macos") {
            vec!["-e".into(), script]
        } else {
            vec![action_hint, text]
        };
        let status = std::process::Command::new(&self.program)
            .args(&args)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        match status {
            Ok(s) if s.success() => logged,
            Ok(s) => logged.and(Err(format!(
                "notify: {} exited with {s}",
                self.program.display()
            ))),
            Err(e) => logged.and(Err(format!(
                "notify: cannot run {}: {e}",
                self.program.display()
            ))),
        }
    }

    fn backend(&self) -> &'static str {
        if cfg!(target_os = "macos") {
            "osascript"
        } else {
            "notify-send"
        }
    }
}

/// The notifier this build uses: the per-OS CLI when present, the log
/// backend otherwise. Never fails to produce *some* backend.
pub fn platform_notifier(notify_log: PathBuf) -> Box<dyn Notifier> {
    match CliNotifier::detect(notify_log.clone()) {
        Some(n) => Box::new(n),
        None => Box::new(LogNotifier::new(notify_log)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::tmpdir;

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
    fn notifier_actions_route_to_the_inbox_protocol() {
        // P7-6: the three buttons are the inbox's own verbs — no new state
        // machine, and a frontend could not tell them apart from the CLI.
        let approve = serde_json::to_value(action_request(NotifyAction::Approve, "c1")).unwrap();
        assert_eq!(approve["method"], "inbox_act");
        assert_eq!(approve["id"], "c1");
        let reject = serde_json::to_value(action_request(NotifyAction::Reject, "c1")).unwrap();
        assert_eq!(reject["method"], "inbox_decide");
        assert_eq!(reject["decision"], "reject");
        assert!(reject["snooze_ms"].is_null(), "reject carries no window");
        let snooze = serde_json::to_value(action_request(NotifyAction::Snooze, "c1")).unwrap();
        assert_eq!(snooze["method"], "inbox_decide");
        assert_eq!(snooze["decision"], "snooze");
        assert_eq!(snooze["snooze_ms"], DEFAULT_SNOOZE_MS);
        assert_eq!(
            [
                NotifyAction::Approve,
                NotifyAction::Reject,
                NotifyAction::Snooze
            ]
            .map(|a| a.label()),
            ["Approve", "Reject", "Snooze"]
        );

        // The log backend records the card *and* the offered actions.
        let dir = tmpdir("notify-log");
        let path = dir.join("notify.jsonl");
        let backend = LogNotifier::new(path.clone());
        let card = Card {
            id: "channel.draft".into(),
            class: "channel.draft".into(),
            title: "outbound local → ops".into(),
            lines: vec!["deploy is done".into()],
            count: 1,
            created_ms: 0,
            expires_ms: CARD_TTL_MS,
            benefit: 50,
        };
        backend
            .notify(&card, &[NotifyAction::Approve, NotifyAction::Snooze])
            .unwrap();
        let rec: serde_json::Value =
            serde_json::from_str(std::fs::read_to_string(&path).unwrap().trim()).unwrap();
        assert_eq!(rec["card"], "channel.draft");
        assert_eq!(rec["actions"][0], "Approve");
        assert_eq!(rec["actions"][1], "Snooze");
        assert!(rec["body"].as_str().unwrap().contains("deploy is done"));

        // Every build has a backend, and it names itself.
        let chosen = platform_notifier(dir.join("notify2.jsonl"));
        assert!(!chosen.backend().is_empty());
        assert!(
            chosen.notify(&card, &[NotifyAction::Approve]).is_ok(),
            "the chosen backend must be usable on this host"
        );

        // The inbox view is the same shape the ctl surface serves.
        let items = vec![item("1", "note.low", 5, "body")];
        let view = inbox_view(&items);
        assert_eq!(view["count"], 1);
        assert_eq!(view["items"][0]["id"], "1");
        assert!(view["items"][0]["class"].as_str().is_some());
        assert!(inbox_view(&[]).as_object().is_some());
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
