//! Agent Inbox (playbook 5.3): every open line between user and agent is
//! a durable item — approve / reject / snooze writes a receipt to the
//! journal. Items are one file each under `inbox/`; state transitions
//! happen via atomic rename so a crash can't leave a torn item.
//!
//! State machine: open → approved | rejected | snoozed(until) | acted.
//! Snoozed items resurface as open when `until_ms` passes — a deferred
//! interruption still lands at a breakpoint, per the HCI literature.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::event::now_ms;
use crate::journal::Journal;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ItemState {
    Open,
    Approved,
    Rejected,
    Snoozed,
    /// Approved item's work was spawned/completed.
    Acted,
}

/// An inbox item — the proactive UX contract: an offer the user can
/// approve (act now), reject (drop), or snooze (resurface later).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxItem {
    pub id: String,
    pub created_ms: u64,
    /// Event class that produced it.
    pub class: String,
    /// Trigger source id.
    pub source: String,
    pub title: String,
    pub body: String,
    /// For act-class items: the prompt an approval would run.
    #[serde(default)]
    pub act_prompt: Option<String>,
    pub state: ItemState,
    /// Snooze wake time; also reused as acted/resolved timestamp.
    #[serde(default)]
    pub until_ms: Option<u64>,
}

pub struct Inbox {
    dir: PathBuf,
}

impl Inbox {
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Self { dir })
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Write atomically: tmp + rename.
    fn write_item(&self, item: &InboxItem) -> std::io::Result<()> {
        let tmp = self.dir.join(format!(".{}.tmp", item.id));
        std::fs::write(&tmp, serde_json::to_vec_pretty(item)?)?;
        std::fs::rename(tmp, self.path(&item.id))
    }

    /// Open a new item and journal it.
    pub fn open(&self, journal: &Journal, item: InboxItem) -> std::io::Result<()> {
        self.write_item(&item)?;
        journal.log(
            "inbox_open",
            serde_json::json!({
                "id": item.id, "class": item.class, "source": item.source,
                "title": item.title,
            }),
        );
        Ok(())
    }

    /// List items, open-snoozed-by-expiry first (resurface), then open,
    /// then resolved — newest first within a state.
    pub fn list(&self) -> Vec<InboxItem> {
        let mut items: Vec<InboxItem> = std::fs::read_dir(&self.dir)
            .map(|rd| {
                rd.flatten()
                    .filter_map(|e| {
                        let name = e.file_name().to_string_lossy().to_string();
                        if !name.ends_with(".json") || name.starts_with('.') {
                            return None;
                        }
                        std::fs::read_to_string(e.path())
                            .ok()
                            .and_then(|t| serde_json::from_str(&t).ok())
                    })
                    .collect()
            })
            .unwrap_or_default();

        let now = now_ms();
        // Resurface expired snoozes.
        for it in items.iter_mut() {
            if it.state == ItemState::Snoozed && it.until_ms.is_some_and(|u| u <= now) {
                it.state = ItemState::Open;
                it.until_ms = None;
                let _ = self.write_item(it);
            }
        }
        let rank = |s: ItemState| match s {
            ItemState::Open => 0,
            ItemState::Snoozed => 1,
            _ => 2,
        };
        items.sort_by_key(|i| (rank(i.state), std::cmp::Reverse(i.created_ms)));
        items
    }

    pub fn get(&self, id: &str) -> Option<InboxItem> {
        // Accept id prefix for CLI convenience.
        let matches: Vec<_> = self
            .list()
            .into_iter()
            .filter(|i| i.id == id || i.id.starts_with(id))
            .collect();
        if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        }
    }

    /// Record a user decision; writes the receipt to the journal.
    pub fn decide(
        &self,
        journal: &Journal,
        id: &str,
        decision: &str,
        snooze_ms: Option<u64>,
    ) -> Result<InboxItem, String> {
        let mut item = self
            .get(id)
            .ok_or_else(|| format!("inbox: no item '{id}'"))?;
        match decision {
            "approve" => item.state = ItemState::Approved,
            "reject" => item.state = ItemState::Rejected,
            "snooze" => {
                item.state = ItemState::Snoozed;
                item.until_ms = Some(now_ms() + snooze_ms.unwrap_or(3_600_000));
            }
            other => return Err(format!("inbox: unknown decision '{other}'")),
        }
        self.write_item(&item).map_err(|e| e.to_string())?;
        journal.log(
            "inbox_receipt",
            serde_json::json!({
                "id": item.id, "decision": decision,
                "until_ms": item.until_ms,
            }),
        );
        Ok(item)
    }

    /// Mark an approved item as acted (spawned/completed), with receipt.
    pub fn mark_acted(&self, journal: &Journal, id: &str) -> std::io::Result<()> {
        if let Some(mut item) = self.get(id) {
            item.state = ItemState::Acted;
            item.until_ms = Some(now_ms());
            self.write_item(&item)?;
            journal.log(
                "inbox_receipt",
                serde_json::json!({"id": id, "decision": "acted"}),
            );
        }
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::now_ms;
    use crate::journal::Journal;
    use crate::test_util::tmpdir;
    use std::path::Path;
    use std::time::Duration;

    fn setup(tag: &str) -> (PathBuf, Inbox, Journal) {
        let root = tmpdir(tag);
        let inbox = Inbox::new(root.join("inbox")).unwrap();
        let journal = Journal::new(root.join("daemon.jsonl"));
        (root, inbox, journal)
    }

    fn mk(id: &str, created_ms: u64, state: ItemState, until_ms: Option<u64>) -> InboxItem {
        InboxItem {
            id: id.to_string(),
            created_ms,
            class: "test".to_string(),
            source: "src".to_string(),
            title: format!("title {id}"),
            body: "body".to_string(),
            act_prompt: None,
            state,
            until_ms,
        }
    }

    fn journal_kinds(root: &Path, kind: &str) -> Vec<serde_json::Value> {
        let path = root.join("daemon.jsonl");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        text.lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some(kind))
            .collect()
    }

    fn assert_no_tmp(root: &Path) {
        let inbox_dir = root.join("inbox");
        for e in std::fs::read_dir(&inbox_dir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            assert!(!name.ends_with(".tmp"), "tmp file left behind: {name}");
        }
    }

    #[test]
    fn open_creates_file_and_journals() {
        let (root, inbox, journal) = setup("open");
        let item = mk("item1", 1000, ItemState::Open, None);
        inbox.open(&journal, item).unwrap();

        let raw = std::fs::read_to_string(root.join("inbox").join("item1.json")).unwrap();
        let stored: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(stored.get("id").and_then(|v| v.as_str()), Some("item1"));

        let lines = journal_kinds(&root, "inbox_open");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].get("id").and_then(|v| v.as_str()), Some("item1"));
        assert_no_tmp(&root);
    }

    #[test]
    fn list_orders_open_snoozed_resolved_newest_first() {
        let (root, inbox, journal) = setup("order");
        let future = now_ms() + Duration::from_secs(3600).as_millis() as u64;
        inbox
            .open(&journal, mk("o_old", 100, ItemState::Open, None))
            .unwrap();
        inbox
            .open(&journal, mk("o_new", 200, ItemState::Open, None))
            .unwrap();
        inbox
            .open(&journal, mk("s1", 900, ItemState::Snoozed, Some(future)))
            .unwrap();
        inbox
            .open(&journal, mk("r_old", 50, ItemState::Rejected, None))
            .unwrap();
        inbox
            .open(&journal, mk("r_new", 1000, ItemState::Approved, None))
            .unwrap();

        let ids: Vec<String> = inbox.list().iter().map(|i| i.id.clone()).collect();
        assert_eq!(ids, vec!["o_new", "o_old", "s1", "r_new", "r_old"]);
        let _ = root;
    }

    #[test]
    fn list_resurfaces_expired_snooze() {
        let (_root, inbox, journal) = setup("resurface");
        inbox
            .open(&journal, mk("sleepy", 100, ItemState::Snoozed, Some(1)))
            .unwrap();

        let listed = inbox.list();
        let item = listed.iter().find(|i| i.id == "sleepy").unwrap();
        assert_eq!(item.state, ItemState::Open);
        assert_eq!(item.until_ms, None);

        let again = inbox.get("sleepy").unwrap();
        assert_eq!(again.state, ItemState::Open);
        assert_eq!(again.until_ms, None);
    }

    #[test]
    fn get_exact_prefix_ambiguous_unknown() {
        let (_root, inbox, journal) = setup("get");
        inbox
            .open(&journal, mk("abc123", 100, ItemState::Open, None))
            .unwrap();
        inbox
            .open(&journal, mk("abc456", 200, ItemState::Open, None))
            .unwrap();
        inbox
            .open(&journal, mk("xyz789", 300, ItemState::Open, None))
            .unwrap();

        assert_eq!(inbox.get("abc123").unwrap().id, "abc123");
        assert_eq!(inbox.get("xyz").unwrap().id, "xyz789");
        assert!(inbox.get("abc").is_none());
        assert!(inbox.get("zzz-nope").is_none());
    }

    #[test]
    fn decide_transitions_and_receipts() {
        let (root, inbox, journal) = setup("decide");
        inbox
            .open(&journal, mk("a1", 100, ItemState::Open, None))
            .unwrap();
        inbox
            .open(&journal, mk("r1", 200, ItemState::Open, None))
            .unwrap();
        inbox
            .open(&journal, mk("s1", 300, ItemState::Open, None))
            .unwrap();

        let a = inbox.decide(&journal, "a1", "approve", None).unwrap();
        assert_eq!(a.state, ItemState::Approved);
        let r = inbox.decide(&journal, "r1", "reject", None).unwrap();
        assert_eq!(r.state, ItemState::Rejected);

        let before = now_ms();
        let s = inbox
            .decide(&journal, "s1", "snooze", Some(60_000))
            .unwrap();
        let after = now_ms();
        assert_eq!(s.state, ItemState::Snoozed);
        let until = s.until_ms.unwrap();
        assert!(until >= before + 60_000 && until <= after + 60_000);

        assert_eq!(inbox.get("a1").unwrap().state, ItemState::Approved);
        assert_eq!(inbox.get("r1").unwrap().state, ItemState::Rejected);
        assert_eq!(inbox.get("s1").unwrap().state, ItemState::Snoozed);

        let receipts = journal_kinds(&root, "inbox_receipt");
        assert_eq!(receipts.len(), 3);
        let dec = |id: &str| {
            receipts
                .iter()
                .find(|v| v.get("id").and_then(|x| x.as_str()) == Some(id))
                .and_then(|v| v.get("decision").and_then(|x| x.as_str()))
                .unwrap()
                .to_string()
        };
        assert_eq!(dec("a1"), "approve");
        assert_eq!(dec("r1"), "reject");
        assert_eq!(dec("s1"), "snooze");
        assert_no_tmp(&root);
    }

    #[test]
    fn decide_errors() {
        let (_root, inbox, journal) = setup("decide-err");
        inbox
            .open(&journal, mk("e1", 100, ItemState::Open, None))
            .unwrap();
        assert!(inbox.decide(&journal, "e1", "frobnicate", None).is_err());
        assert!(inbox.decide(&journal, "missing", "approve", None).is_err());
        assert_eq!(inbox.get("e1").unwrap().state, ItemState::Open);
    }

    #[test]
    fn mark_acted_sets_state_timestamp_receipt() {
        let (root, inbox, journal) = setup("acted");
        inbox
            .open(&journal, mk("w1", 100, ItemState::Approved, None))
            .unwrap();
        let before = now_ms();
        inbox.mark_acted(&journal, "w1").unwrap();
        let after = now_ms();

        let item = inbox.get("w1").unwrap();
        assert_eq!(item.state, ItemState::Acted);
        let ts = item.until_ms.unwrap();
        assert!(ts >= before && ts <= after);

        let receipts = journal_kinds(&root, "inbox_receipt");
        assert_eq!(receipts.len(), 1);
        assert_eq!(
            receipts[0].get("decision").and_then(|v| v.as_str()),
            Some("acted")
        );
        assert_no_tmp(&root);
    }

    #[test]
    fn no_tmp_files_remain_after_writes() {
        let (root, inbox, journal) = setup("crashsafe");
        inbox
            .open(&journal, mk("c1", 100, ItemState::Open, None))
            .unwrap();
        inbox.decide(&journal, "c1", "approve", None).unwrap();
        inbox.mark_acted(&journal, "c1").unwrap();
        assert_no_tmp(&root);
    }
}
