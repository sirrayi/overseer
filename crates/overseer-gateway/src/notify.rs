//! Notification tiers (playbook §2.4): silent → inbox → push.
//! "Never push what could be a digest line." Push v1 appends to
//! `push.jsonl` — frontends (TUI/desktop/messaging) tail it; a real
//! OS-notification channel lands with the desktop frontend.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

use crate::event::now_ms;

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
#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("overseer-gateway-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
