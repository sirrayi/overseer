//! Append-only daemon journal — every pipeline decision and inbox
//! transition is a JSONL record (Invariant 1 discipline applied to the
//! daemon: state is a view over the journal).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

use serde::Serialize;

use crate::event::now_ms;

/// One journal record. `kind` discriminates: trigger_fired, dedup_drop,
/// triage, route, inbox_open, inbox_receipt, spawn, spawn_exit,
/// kill_switch, heartbeat.
#[derive(Debug, Serialize)]
pub struct JournalEntry<'a> {
    pub ts_ms: u64,
    pub kind: &'a str,
    #[serde(flatten)]
    pub fields: serde_json::Value,
}

/// Cheap append-only writer — opens per append (journal volume is low;
/// correctness over buffering, and a crashed daemon leaves a clean tail).
pub struct Journal {
    path: PathBuf,
}

impl Journal {
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    pub fn log(&self, kind: &str, fields: serde_json::Value) {
        let entry = JournalEntry {
            ts_ms: now_ms(),
            kind,
            fields,
        };
        let Ok(mut line) = serde_json::to_string(&entry) else {
            return;
        };
        line.push('\n');
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .read(true)
            .open(&self.path)
        {
            // S-G1 torn-tail repair: a crashed writer may have left a partial
            // line without trailing newline; appending directly would fuse it
            // with this record into one unparseable line and LOSE this append.
            // Terminate the partial tail first so each record stays one line.
            use std::io::{Read, Seek, SeekFrom};
            if let Ok(len) = f.metadata().map(|m| m.len()) {
                if len > 0 {
                    let mut last = [0u8; 1];
                    if f.seek(SeekFrom::End(-1)).is_ok()
                        && f.read_exact(&mut last).is_ok()
                        && last[0] != b'\n'
                    {
                        let _ = f.write_all(b"\n");
                    }
                }
            }
            let _ = f.write_all(line.as_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::tmpdir;
    use std::io::Write;

    fn read_kinds(dir: &std::path::Path) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(dir.join("daemon.jsonl")).expect("read journal");
        text.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .collect()
    }

    #[test]
    fn log_appends_one_json_line_per_call_with_kind_and_flat_fields() {
        let dir = tmpdir("append");
        let path = dir.join("daemon.jsonl");
        let _ = std::fs::remove_file(&path);
        let j = Journal::new(path.clone());
        j.log(
            "trigger_fired",
            serde_json::json!({"source": "watch-inbox"}),
        );
        j.log(
            "triage",
            serde_json::json!({"class": "git.dirty", "score": 3}),
        );

        let text = std::fs::read_to_string(&path).expect("read journal");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("valid json");
        assert_eq!(first["kind"], "trigger_fired");
        assert_eq!(first["source"], "watch-inbox");
        assert!(first.get("ts_ms").and_then(|v| v.as_u64()).is_some());
        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("valid json");
        assert_eq!(second["kind"], "triage");
        assert_eq!(second["class"], "git.dirty");
    }

    #[test]
    fn torn_tail_line_skipped_by_parse_filter() {
        let dir = tmpdir("torn-tail");
        let path = dir.join("daemon.jsonl");
        let _ = std::fs::remove_file(&path);
        let j = Journal::new(path.clone());
        j.log("heartbeat", serde_json::json!({"note": "one"}));
        j.log("heartbeat", serde_json::json!({"note": "two"}));
        // Simulate a crashed write: partial bytes with no trailing newline.
        {
            let mut f = OpenOptions::new().append(true).open(&path).expect("open");
            f.write_all(b"{\"kind\": \"torn").expect("write partial");
        }

        let entries = read_kinds(&dir);
        let hearts: Vec<&serde_json::Value> = entries
            .iter()
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("heartbeat"))
            .collect();
        assert_eq!(hearts.len(), 2);
        assert_eq!(hearts[0]["note"], "one");
        assert_eq!(hearts[1]["note"], "two");
    }

    #[test]
    fn torn_tail_followed_by_append_keeps_next_record() {
        // S-G1 crash-restart shape: torn bytes then the next log() call.
        // Without the repair the torn prefix fuses with the new record and
        // the post-crash append is silently lost.
        let dir = tmpdir("torn-tail-append");
        let path = dir.join("daemon.jsonl");
        let _ = std::fs::remove_file(&path);
        let j = Journal::new(path.clone());
        j.log("heartbeat", serde_json::json!({"note": "one"}));
        j.log("heartbeat", serde_json::json!({"note": "two"}));
        {
            let mut f = OpenOptions::new().append(true).open(&path).expect("open");
            f.write_all(b"{\"kind\": \"torn").expect("write partial");
        }
        j.log("heartbeat", serde_json::json!({"note": "three"}));
        let entries = read_kinds(&dir);
        let hearts: Vec<&serde_json::Value> = entries
            .iter()
            .filter(|v| v.get("kind").and_then(|k| k.as_str()) == Some("heartbeat"))
            .collect();
        assert_eq!(hearts.len(), 3, "post-crash append must survive");
        assert_eq!(hearts[2]["note"], "three");
    }
}
