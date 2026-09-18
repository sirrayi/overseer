//! Trigger sources — cron-ish intervals, file watchers, heartbeats.
//!
//! All sources are pull-based against the daemon tick: no OS callbacks,
//! no threads per trigger, nothing that can wedge. A watcher polls mtimes
//! (cheap `read_dir`, no `notify` dep) and emits at most one event per
//! changed file per scan.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::TriggerSpec;
use crate::event::{now_ms, TriggerEvent};

/// Runtime state for one configured trigger.
pub enum Trigger {
    Interval {
        id: String,
        every_ms: u64,
        next_ms: u64,
        body: String,
        class: String,
    },
    Watch {
        id: String,
        dir: PathBuf,
        name_contains: String,
        body: String,
        class: String,
        /// path → last seen mtime (ms); seeds on first scan so existing
        /// files don't storm the inbox at daemon start.
        mtimes: HashMap<PathBuf, u64>,
        primed: bool,
    },
    Heartbeat {
        id: String,
        every_ms: u64,
        next_ms: u64,
    },
}

impl Trigger {
    pub fn from_spec(spec: &TriggerSpec) -> Self {
        let now = now_ms();
        match spec {
            TriggerSpec::Interval {
                id,
                every_s,
                body,
                class,
            } => Trigger::Interval {
                id: id.clone(),
                every_ms: every_s * 1000,
                next_ms: now + every_s * 1000,
                body: body.clone(),
                class: if class.is_empty() {
                    "interval".to_string()
                } else {
                    class.clone()
                },
            },
            TriggerSpec::Watch {
                id,
                dir,
                name_contains,
                body,
                class,
            } => Trigger::Watch {
                id: id.clone(),
                dir: dir.clone(),
                name_contains: name_contains.clone(),
                body: body.clone(),
                class: if class.is_empty() {
                    "fs.change".to_string()
                } else {
                    class.clone()
                },
                mtimes: HashMap::new(),
                primed: false,
            },
            TriggerSpec::Heartbeat { id, every_s } => Trigger::Heartbeat {
                id: id.clone(),
                every_ms: every_s * 1000,
                next_ms: now + every_s * 1000,
            },
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Trigger::Interval { id, .. }
            | Trigger::Watch { id, .. }
            | Trigger::Heartbeat { id, .. } => id,
        }
    }

    /// Poll the trigger; returns any events due this tick.
    pub fn poll(&mut self) -> Vec<TriggerEvent> {
        let now = now_ms();
        match self {
            Trigger::Interval {
                id,
                every_ms,
                next_ms,
                body,
                class,
            } => {
                if now < *next_ms {
                    return Vec::new();
                }
                // Catch-up: skip missed periods rather than storming.
                *next_ms = now + *every_ms;
                vec![TriggerEvent::new(id.as_str(), class.clone(), body.clone())]
            }
            Trigger::Heartbeat {
                id,
                every_ms,
                next_ms,
            } => {
                if now < *next_ms {
                    return Vec::new();
                }
                *next_ms = now + *every_ms;
                vec![TriggerEvent::new(id.as_str(), "heartbeat", "check-in")]
            }
            Trigger::Watch {
                id,
                dir,
                name_contains,
                body,
                class,
                mtimes,
                primed,
            } => poll_watch(id, dir, name_contains, body, class, mtimes, primed),
        }
    }
}

fn mtime_ms(path: &Path) -> u64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[allow(clippy::too_many_arguments)]
fn poll_watch(
    id: &str,
    dir: &Path,
    name_contains: &str,
    body: &str,
    class: &str,
    mtimes: &mut HashMap<PathBuf, u64>,
    primed: &mut bool,
) -> Vec<TriggerEvent> {
    let mut out = Vec::new();
    let mut live = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_file() {
                continue;
            }
            let name = e.file_name().to_string_lossy().to_string();
            if !name_contains.is_empty() && !name.contains(name_contains) {
                continue;
            }
            let m = mtime_ms(&path);
            live.push(path.clone());
            match mtimes.get(&path) {
                Some(prev) if *prev == m => {}
                Some(_) => {
                    mtimes.insert(path.clone(), m);
                    out.push(TriggerEvent::new(
                        id,
                        class.to_string(),
                        format!("{body}: {}", name),
                    ));
                }
                None => {
                    mtimes.insert(path.clone(), m);
                    if *primed {
                        out.push(TriggerEvent::new(
                            id,
                            class.to_string(),
                            format!("{body}: {}", name),
                        ));
                    }
                }
            }
        }
    }
    // Forget deleted files so a re-created file fires as new.
    mtimes.retain(|p, _| live.contains(p));
    *primed = true;
    out
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

    fn interval_spec(id: &str, every_s: u64) -> TriggerSpec {
        TriggerSpec::Interval {
            id: id.to_string(),
            every_s,
            body: "body".to_string(),
            class: String::new(),
        }
    }

    #[test]
    fn interval_zero_period_fires_immediately() {
        let mut trigger = Trigger::from_spec(&interval_spec("int-zero", 0));
        assert_eq!(trigger.id(), "int-zero");
        let events = trigger.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "int-zero");
        assert_eq!(events[0].class, "interval");
        assert_eq!(events[0].payload, "body");
    }

    #[test]
    fn interval_respects_period() {
        let mut trigger = Trigger::from_spec(&interval_spec("int-hourly", 3600));
        assert!(trigger.poll().is_empty());
        let mut overdue = Trigger::Interval {
            id: "int-overdue".to_string(),
            every_ms: 3_600_000,
            next_ms: now_ms().saturating_sub(1),
            body: "body".to_string(),
            class: "interval".to_string(),
        };
        assert_eq!(overdue.poll().len(), 1);
        assert!(
            overdue.poll().is_empty(),
            "catch-up must advance past the fired period"
        );
    }

    #[test]
    fn heartbeat_fires_on_schedule() {
        let mut trigger = Trigger::from_spec(&TriggerSpec::Heartbeat {
            id: "hb-zero".to_string(),
            every_s: 0,
        });
        assert_eq!(trigger.id(), "hb-zero");
        let events = trigger.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "hb-zero");
        assert_eq!(events[0].class, "heartbeat");
        let mut quiet = Trigger::from_spec(&TriggerSpec::Heartbeat {
            id: "hb-hourly".to_string(),
            every_s: 3600,
        });
        assert!(quiet.poll().is_empty());
    }

    #[test]
    fn watch_primes_then_fires_on_new_and_changed_file() {
        let dir = tmpdir("trigger-watch");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut trigger = Trigger::from_spec(&TriggerSpec::Watch {
            id: "watch".to_string(),
            dir: dir.clone(),
            name_contains: String::new(),
            body: "changed".to_string(),
            class: String::new(),
        });
        assert_eq!(trigger.id(), "watch");
        assert!(trigger.poll().is_empty(), "first scan only primes");
        let file = dir.join("note.txt");
        std::fs::write(&file, "v1").unwrap();
        let events = trigger.poll();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].source, "watch");
        assert!(trigger.poll().is_empty(), "steady state is quiet");
        let before = mtime_ms(&file);
        let mut n = 0u32;
        while mtime_ms(&file) == before && n < 100_000 {
            n += 1;
            std::fs::write(&file, format!("v2-{n}")).unwrap();
        }
        assert_ne!(
            mtime_ms(&file),
            before,
            "content rewrite must advance mtime"
        );
        assert_eq!(trigger.poll().len(), 1);
    }
}
