//! Two-phase outbound messaging (P7-5): draft → approve → send.
//!
//! The approval ladder keeps `external → Ask`; this module is the durable
//! half of that verdict. A draft is a file, approval is a state transition,
//! and the send carries an idempotency key so a retry (or a double-click on
//! Approve) can never post the same message twice. Every transition writes
//! a journal receipt, mirroring the inbox.
//!
//! Sending is impossible without an approval: [`Outbox::send_approved`]
//! refuses a draft that has not been approved, and the transport is passed
//! in by the caller so this module owns the *protocol*, not the channel.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::event::now_ms;
use crate::journal::Journal;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DraftState {
    Draft,
    Approved,
    Sent,
    Rejected,
}

/// One outbound message, waiting for a human verdict.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Draft {
    pub id: String,
    pub created_ms: u64,
    /// Recipient on the channel (chat id, handle, address).
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread: Option<String>,
    pub text: String,
    /// Channel transport ("local", "telegram", …).
    #[serde(default = "default_channel")]
    pub channel: String,
    pub state: DraftState,
    /// Stable across retries: the transport can dedupe on it, so a
    /// re-send after a crash cannot post the message twice.
    pub idempotency_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sent_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

fn default_channel() -> String {
    "local".to_string()
}

/// The outcome of an approval: `sent: false` means the draft was already
/// sent (or rejected) and nothing went out — the idempotent path.
#[derive(Debug, Clone)]
pub struct SendOutcome {
    pub draft: Draft,
    pub sent: bool,
    /// Set when a previous send attempt failed and this one retried.
    pub retried: bool,
}

/// Transport seam: the daemon supplies a real channel, tests supply a
/// recorder. `send` must treat `draft.idempotency_key` as the dedup key.
pub trait Sender {
    fn send(&self, draft: &Draft) -> Result<(), String>;
}

/// A boxed transport is itself a transport — the daemon picks one at the
/// approval boundary, where the draft's channel is known.
impl Sender for Box<dyn Sender> {
    fn send(&self, draft: &Draft) -> Result<(), String> {
        (**self).send(draft)
    }
}

/// A local delivery sink: the outbound equivalent of `push.jsonl`
/// (frontends tail it). Records the idempotency key so a consumer can
/// dedupe exactly as a remote channel would.
pub struct LogSender {
    path: PathBuf,
}

impl LogSender {
    pub fn new(path: PathBuf) -> Self {
        LogSender { path }
    }
}

impl Sender for LogSender {
    fn send(&self, draft: &Draft) -> Result<(), String> {
        use std::io::Write;
        let rec = serde_json::json!({
            "ts_ms": now_ms(),
            "id": draft.id,
            "idempotency_key": draft.idempotency_key,
            "to": draft.to,
            "thread": draft.thread,
            "channel": draft.channel,
            "text": draft.text,
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
            .map_err(|e| format!("outbox: local sink {}: {e}", self.path.display()))
    }
}

pub struct Outbox {
    dir: PathBuf,
}

impl Outbox {
    pub fn new(dir: PathBuf) -> std::io::Result<Self> {
        std::fs::create_dir_all(&dir)?;
        Ok(Outbox { dir })
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    /// Atomic write: tmp + rename, so a crash never leaves a torn draft.
    fn write(&self, draft: &Draft) -> std::io::Result<()> {
        let tmp = self.dir.join(format!(".{}.tmp", draft.id));
        std::fs::write(&tmp, serde_json::to_vec_pretty(draft)?)?;
        std::fs::rename(tmp, self.path(&draft.id))
    }

    pub fn list(&self) -> Vec<Draft> {
        let mut drafts: Vec<Draft> = std::fs::read_dir(&self.dir)
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
        drafts.sort_by_key(|d| std::cmp::Reverse(d.created_ms));
        drafts
    }

    /// Accept an id prefix, like the inbox does, for CLI convenience.
    pub fn get(&self, id: &str) -> Option<Draft> {
        let matches: Vec<Draft> = self
            .list()
            .into_iter()
            .filter(|d| d.id == id || d.id.starts_with(id))
            .collect();
        if matches.len() == 1 {
            matches.into_iter().next()
        } else {
            None
        }
    }

    /// Phase one: record the message as a draft. Nothing is sent.
    pub fn draft(
        &self,
        journal: &Journal,
        channel: &str,
        to: &str,
        thread: Option<&str>,
        text: &str,
    ) -> Result<Draft, String> {
        if text.trim().is_empty() {
            return Err("outbox: refusing to draft an empty message".into());
        }
        let draft = Draft {
            id: uuid::Uuid::now_v7().to_string(),
            created_ms: now_ms(),
            to: to.to_string(),
            thread: thread.map(str::to_string),
            text: text.to_string(),
            channel: if channel.trim().is_empty() {
                default_channel()
            } else {
                channel.to_string()
            },
            state: DraftState::Draft,
            idempotency_key: uuid::Uuid::now_v7().to_string(),
            sent_ms: None,
            error: None,
        };
        self.write(&draft).map_err(|e| e.to_string())?;
        journal.log(
            "channel.draft",
            serde_json::json!({
                "id": draft.id, "channel": draft.channel, "to": draft.to,
                "thread": draft.thread, "bytes": draft.text.len(),
            }),
        );
        Ok(draft)
    }

    /// Phase two, step one: mark approved (idempotent).
    pub fn approve(&self, journal: &Journal, id: &str) -> Result<Draft, String> {
        let mut draft = self.get(id).ok_or_else(|| format!("outbox: no draft '{id}'"))?;
        match draft.state {
            DraftState::Approved | DraftState::Sent => return Ok(draft),
            DraftState::Rejected => {
                return Err(format!("outbox: draft '{id}' was rejected — a new draft is needed"))
            }
            DraftState::Draft => {}
        }
        draft.state = DraftState::Approved;
        self.write(&draft).map_err(|e| e.to_string())?;
        journal.log(
            "channel.approved",
            serde_json::json!({"id": draft.id, "channel": draft.channel, "to": draft.to}),
        );
        Ok(draft)
    }

    pub fn reject(&self, journal: &Journal, id: &str) -> Result<Draft, String> {
        let mut draft = self.get(id).ok_or_else(|| format!("outbox: no draft '{id}'"))?;
        if draft.state == DraftState::Sent {
            return Err(format!("outbox: draft '{id}' was already sent"));
        }
        draft.state = DraftState::Rejected;
        self.write(&draft).map_err(|e| e.to_string())?;
        journal.log(
            "channel.rejected",
            serde_json::json!({"id": draft.id, "to": draft.to}),
        );
        Ok(draft)
    }

    /// Phase two, step two: send an *approved* draft through `sender`.
    /// Sending an unapproved draft is impossible by construction.
    pub fn send_approved(
        &self,
        journal: &Journal,
        id: &str,
        sender: &dyn Sender,
    ) -> Result<SendOutcome, String> {
        let mut draft = self.get(id).ok_or_else(|| format!("outbox: no draft '{id}'"))?;
        match draft.state {
            DraftState::Sent => {
                return Ok(SendOutcome {
                    draft,
                    sent: false,
                    retried: false,
                })
            }
            DraftState::Draft => {
                return Err(format!(
                    "outbox: draft '{id}' is not approved — sending without approval is refused"
                ))
            }
            DraftState::Rejected => {
                return Err(format!("outbox: draft '{id}' was rejected"))
            }
            DraftState::Approved => {}
        }
        let retried = draft.error.is_some();
        match sender.send(&draft) {
            Ok(()) => {
                draft.state = DraftState::Sent;
                draft.sent_ms = Some(now_ms());
                draft.error = None;
                self.write(&draft).map_err(|e| e.to_string())?;
                journal.log(
                    "channel.sent",
                    serde_json::json!({
                        "id": draft.id, "channel": draft.channel, "to": draft.to,
                        "idempotency_key": draft.idempotency_key,
                        "retried": retried,
                    }),
                );
                Ok(SendOutcome {
                    draft,
                    sent: true,
                    retried,
                })
            }
            Err(e) => {
                // Stay Approved: the operator can retry, and the recorded
                // idempotency key keeps the retry deduplicable.
                draft.error = Some(e.clone());
                self.write(&draft).map_err(|e| e.to_string())?;
                journal.log(
                    "channel.send_failed",
                    serde_json::json!({
                        "id": draft.id, "channel": draft.channel, "to": draft.to,
                        "error": e,
                    }),
                );
                Err(format!("outbox: send failed: {e}"))
            }
        }
    }

    /// The whole approved path, for one-shot approval surfaces.
    pub fn approve_and_send(
        &self,
        journal: &Journal,
        id: &str,
        sender: &dyn Sender,
    ) -> Result<SendOutcome, String> {
        self.approve(journal, id)?;
        self.send_approved(journal, id, sender)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overseer-outbox-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn journal(dir: &std::path::Path) -> Journal {
        Journal::new(dir.join("daemon.jsonl"))
    }

    fn kinds(dir: &std::path::Path) -> Vec<String> {
        std::fs::read_to_string(dir.join("daemon.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .filter_map(|v| v.get("kind").and_then(|k| k.as_str()).map(str::to_string))
            .collect()
    }

    /// Records every send attempt, keyed by idempotency key.
    #[derive(Default)]
    struct Recorder {
        sent: Mutex<Vec<(String, String)>>,
        fail: bool,
    }

    impl Sender for Recorder {
        fn send(&self, draft: &Draft) -> Result<(), String> {
            if self.fail {
                return Err("transport down".into());
            }
            self.sent
                .lock()
                .unwrap()
                .push((draft.idempotency_key.clone(), draft.text.clone()));
            Ok(())
        }
    }

    #[test]
    fn sending_without_approval_is_impossible() {
        let dir = tmpdir("unapproved");
        let j = journal(&dir);
        let outbox = Outbox::new(dir.join("outbox")).unwrap();
        let draft = outbox
            .draft(&j, "local", "ops", Some("t1"), "deploy is done")
            .unwrap();
        assert_eq!(draft.state, DraftState::Draft);
        let rec = Recorder::default();
        // A draft cannot be sent…
        let err = outbox.send_approved(&j, &draft.id, &rec).unwrap_err();
        assert!(err.contains("not approved"), "got: {err}");
        assert!(rec.sent.lock().unwrap().is_empty(), "nothing left the box");
        // …and must be approved first.
        let approved = outbox.approve(&j, &draft.id).unwrap();
        assert_eq!(approved.state, DraftState::Approved);
        let outcome = outbox.send_approved(&j, &draft.id, &rec).unwrap();
        assert!(outcome.sent);
        assert_eq!(outcome.draft.state, DraftState::Sent);
        assert_eq!(rec.sent.lock().unwrap().len(), 1);
        // A rejected draft never sends.
        let second = outbox
            .draft(&j, "local", "ops", None, "second")
            .unwrap();
        outbox.reject(&j, &second.id).unwrap();
        let err = outbox
            .approve_and_send(&j, &second.id, &rec)
            .unwrap_err();
        assert!(err.contains("rejected"), "got: {err}");
        assert_eq!(rec.sent.lock().unwrap().len(), 1);
        // Receipts for every transition.
        let seen = kinds(&dir);
        assert!(seen.contains(&"channel.draft".to_string()));
        assert!(seen.contains(&"channel.approved".to_string()));
        assert!(seen.contains(&"channel.sent".to_string()));
        assert!(seen.contains(&"channel.rejected".to_string()));
        // State survives a reopen (durable, not in-memory).
        let reopened = Outbox::new(dir.join("outbox")).unwrap();
        assert_eq!(reopened.get(&draft.id).unwrap().state, DraftState::Sent);
        let _ = j;
    }

    #[test]
    fn double_approval_sends_once_with_a_stable_idempotency_key() {
        let dir = tmpdir("idempotent");
        let j = journal(&dir);
        let outbox = Outbox::new(dir.join("outbox")).unwrap();
        let draft = outbox.draft(&j, "telegram", "77", None, "on my way").unwrap();
        let key = draft.idempotency_key.clone();
        let rec = Recorder::default();

        let first = outbox.approve_and_send(&j, &draft.id, &rec).unwrap();
        assert!(first.sent);
        assert!(!first.retried);
        // Second approval: no second send, same key reported.
        let second = outbox.approve_and_send(&j, &draft.id, &rec).unwrap();
        assert!(!second.sent, "double approve must not send again");
        assert_eq!(second.draft.idempotency_key, key);
        let sends = rec.sent.lock().unwrap();
        assert_eq!(sends.len(), 1);
        assert_eq!(sends[0].0, key);
        drop(sends);
        // Exactly one `channel_sent` receipt.
        assert_eq!(
            kinds(&dir).iter().filter(|k| *k == "channel.sent").count(),
            1
        );
        // The draft id is its file name: atomic tmp+rename leaves no tmp.
        let files: Vec<String> = std::fs::read_dir(dir.join("outbox"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        assert!(files.iter().all(|f| !f.starts_with('.')), "no tmp left: {files:?}");
        assert_eq!(files.len(), 1);
        // Empty messages are refused before anything is written.
        assert!(outbox.draft(&j, "local", "ops", None, "   ").is_err());
        assert_eq!(outbox.list().len(), 1);
    }

    #[test]
    fn failed_send_stays_approved_and_retries_with_the_same_key() {
        let dir = tmpdir("retry");
        let j = journal(&dir);
        let outbox = Outbox::new(dir.join("outbox")).unwrap();
        let draft = outbox.draft(&j, "telegram", "77", None, "hello").unwrap();
        let key = draft.idempotency_key.clone();
        let failing = Recorder {
            sent: Mutex::new(Vec::new()),
            fail: true,
        };
        let err = outbox.approve_and_send(&j, &draft.id, &failing).unwrap_err();
        assert!(err.contains("transport down"), "got: {err}");
        let after = outbox.get(&draft.id).unwrap();
        assert_eq!(after.state, DraftState::Approved, "retryable, not lost");
        assert!(after.error.is_some());
        assert!(kinds(&dir).contains(&"channel.send_failed".to_string()));

        // A retry (same approval) succeeds and is flagged as a retry, with
        // the *same* key so the transport can dedupe.
        let ok = Recorder::default();
        let outcome = outbox.send_approved(&j, &draft.id, &ok).unwrap();
        assert!(outcome.sent && outcome.retried);
        assert_eq!(outcome.draft.idempotency_key, key);
        assert_eq!(ok.sent.lock().unwrap().len(), 1);
        assert!(outcome.draft.sent_ms.is_some());
        assert!(outcome.draft.error.is_none(), "the error clears on success");
    }

    #[test]
    fn local_sink_appends_one_json_line_per_delivery() {
        let dir = tmpdir("sink");
        let j = journal(&dir);
        let outbox = Outbox::new(dir.join("outbox")).unwrap();
        let sink = LogSender::new(dir.join("outbox/sent.jsonl"));
        let draft = outbox.draft(&j, "local", "ops", Some("t1"), "first").unwrap();
        outbox.approve_and_send(&j, &draft.id, &sink).unwrap();
        let draft2 = outbox.draft(&j, "local", "ops", None, "second").unwrap();
        outbox.approve_and_send(&j, &draft2.id, &sink).unwrap();
        let text = std::fs::read_to_string(dir.join("outbox/sent.jsonl")).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["text"], "first");
        assert_eq!(lines[0]["channel"], "local");
        assert!(lines[0]["idempotency_key"].as_str().is_some_and(|k| !k.is_empty()));
    }
}
