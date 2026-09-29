//! Thread → session routing (P7-4).
//!
//! One session directory per (channel, thread): a group chat and a DM must
//! never share context, and neither must two rooms on the same channel. The
//! map is persisted under the daemon's channel dir, so a restart (or a
//! reload) resumes the same directories instead of forking new ones.
//!
//! Routing creates the directory (best-effort) so the conversation's home
//! exists the moment the first message arrives — an operator can see which
//! threads the daemon is following, and a later spawn has a place to run.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Persisted map: `channel/thread` key → session dir name.
#[derive(Debug, Default, Serialize, Deserialize)]
struct RouteFile {
    #[serde(default)]
    routes: BTreeMap<String, String>,
}

pub struct ThreadRoutes {
    root: PathBuf,
    file: PathBuf,
    routes: BTreeMap<String, String>,
}

impl ThreadRoutes {
    /// Open (or create) the router under `<daemon>/channels/`.
    pub fn open(root: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(root)?;
        let file = root.join("routes.json");
        let routes = std::fs::read_to_string(&file)
            .ok()
            .and_then(|t| serde_json::from_str::<RouteFile>(&t).ok())
            .map(|f| f.routes)
            .unwrap_or_default();
        Ok(ThreadRoutes {
            root: root.to_path_buf(),
            file,
            routes,
        })
    }

    fn key(channel: &str, thread: Option<&str>) -> String {
        format!("{}/{}", channel, thread.unwrap_or("default"))
    }

    /// The session dir for a thread, assigning one on first sight. Stable
    /// for the life of the map file: the same thread always lands in the
    /// same directory, and that directory exists from the first message.
    pub fn route(&mut self, channel: &str, thread: Option<&str>) -> PathBuf {
        let key = Self::key(channel, thread);
        if let Some(name) = self.routes.get(&key) {
            let dir = self.root.join(name);
            let _ = std::fs::create_dir_all(&dir);
            return dir;
        }
        let mut name = format!("{}-{}", slug(channel), slug(thread.unwrap_or("default")));
        // Deterministic de-collision: two different raw ids that slug to
        // the same name get `-2`, `-3`, … in first-seen order (the map is
        // persisted, so the suffix never moves afterwards).
        if self.routes.values().any(|v| *v == name) {
            let mut n = 2;
            while self.routes.values().any(|v| *v == format!("{name}-{n}")) {
                n += 1;
            }
            name = format!("{name}-{n}");
        }
        self.routes.insert(key, name.clone());
        let _ = self.persist();
        let dir = self.root.join(name);
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// The assigned dir name, if this thread has been seen.
    pub fn dir_of(&self, channel: &str, thread: Option<&str>) -> Option<String> {
        self.routes.get(&Self::key(channel, thread)).cloned()
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }

    fn persist(&self) -> std::io::Result<()> {
        let file = RouteFile {
            routes: self.routes.clone(),
        };
        let text = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        // tmp + rename: a crash mid-write must not truncate the map.
        let tmp = self.file.with_extension("json.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(tmp, &self.file)
    }
}

/// Filesystem-safe slug: lowercase alphanumerics, `-`, `_`, `.` survive;
/// everything else collapses to `-`. Bounded so a long thread id can't
/// produce an unbounded path component.
fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for c in s.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
            out.push(c);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    // Never emit a component that could climb (`..`) or that starts with a
    // separator-ish character.
    let trimmed: String = out
        .trim_matches(|c| c == '-' || c == '.')
        .chars()
        .take(48)
        .collect();
    if trimmed.is_empty() {
        return "x".to_string();
    }
    trimmed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::tmpdir;

    #[test]
    fn two_threads_route_to_two_session_dirs() {
        let root = tmpdir("threads");
        let mut routes = ThreadRoutes::open(&root.join("channels")).unwrap();
        let a = routes.route("telegram", Some("-1001"));
        let b = routes.route("telegram", Some("-1002"));
        let dm = routes.route("telegram", None);
        assert_ne!(a, b, "two threads must not share a session");
        assert_ne!(a, dm, "a DM is its own thread");
        assert_eq!(routes.len(), 3);
        // Both thread homes exist on disk from the first message.
        assert!(
            a.is_dir() && b.is_dir() && dm.is_dir(),
            "session dirs exist"
        );
        // Same thread, same dir — and the map survives a reopen.
        assert_eq!(routes.route("telegram", Some("-1001")), a);
        let mut reopened = ThreadRoutes::open(&root.join("channels")).unwrap();
        assert_eq!(reopened.route("telegram", Some("-1001")), a);
        assert_eq!(reopened.route("telegram", Some("-1002")), b);
        assert_eq!(reopened.len(), 3);
        // A different channel never collides with the same thread id.
        let andere = reopened.route("matrix", Some("-1001"));
        assert_ne!(andere, a);
        // Paths are shaped as <root>/<channel>-<thread>.
        assert!(a.ends_with("telegram-1001"), "got {}", a.display());
        // Slug de-collision keeps distinct raw ids distinct.
        let mut r2 = ThreadRoutes::open(&tmpdir("threads-slug").join("channels")).unwrap();
        let x = r2.route("c", Some("a b"));
        let y = r2.route("c", Some("a/b"));
        assert_ne!(x, y, "slug collision must not merge threads: {x:?} {y:?}");
        assert!(
            y.to_string_lossy().ends_with("-2"),
            "second id collides on slug, got {}",
            y.display()
        );
        // Unhealthy ids still produce usable component names.
        assert!(!slug("").is_empty());
        assert_eq!(slug("../../etc/passwd"), "etc-passwd");
    }
}
