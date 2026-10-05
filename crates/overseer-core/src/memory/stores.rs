//! Memory v2 store resolution: where the user and project stores live.
//!
//! The user store is `<overseer home>/memory`; the project store is
//! `<overseer home>/projects/<slug>-<hash8>/memory`, keyed by the canonical
//! git toplevel (found by walking up for `.git`, no process spawn) or the
//! canonical cwd outside a repository.

use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Which store a note lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    User,
    Project,
}

impl Scope {
    pub const fn name(self) -> &'static str {
        match self {
            Scope::User => "user",
            Scope::Project => "project",
        }
    }

    pub fn parse(name: &str) -> Option<Scope> {
        [Scope::User, Scope::Project]
            .into_iter()
            .find(|s| s.name() == name)
    }
}

/// The Overseer home: `$OVERSEER_HOME` if set, else `$HOME/.overseer`.
/// None when neither is set.
pub fn overseer_home() -> Option<PathBuf> {
    home_from(|k| std::env::var_os(k))
}

fn home_from(get: impl Fn(&str) -> Option<std::ffi::OsString>) -> Option<PathBuf> {
    if let Some(h) = get("OVERSEER_HOME").filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(h));
    }
    get("HOME")
        .filter(|v| !v.is_empty())
        .map(|h| PathBuf::from(h).join(".overseer"))
}

pub fn user_store(home: &Path) -> PathBuf {
    home.join("memory")
}

/// The project key for `cwd`: the nearest ancestor holding a `.git` dir or
/// file (worktrees and submodules use a file), else `cwd` itself — both
/// canonicalized first.
pub fn project_key(cwd: &Path) -> PathBuf {
    let canon = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    canon
        .ancestors()
        .find(|a| a.join(".git").exists())
        .map_or_else(|| canon.clone(), Path::to_path_buf)
}

pub fn project_store(home: &Path, cwd: &Path) -> PathBuf {
    let key = project_key(cwd);
    home.join("projects")
        .join(format!("{}-{}", slug(&key), hash8(&key)))
        .join("memory")
}

/// The key's basename as a [`slugify`] slug of at most 32 chars
/// (`root` when nothing survives).
fn slug(key: &Path) -> String {
    let base = key
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    let out = slugify(&base, 32);
    if out.is_empty() {
        "root".into()
    } else {
        out
    }
}

/// `s` lowercased to `[a-z0-9-]`: other chars become `-`, runs collapse,
/// edges are trimmed; at most `max` chars.
pub fn slugify(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        let c = if c.is_ascii_alphanumeric() { c } else { '-' };
        if c != '-' || !out.is_empty() && !out.ends_with('-') {
            out.push(c);
        }
    }
    out.truncate(max);
    out.trim_end_matches('-').to_string()
}

/// First 8 hex chars of sha256 over the key path's bytes.
fn hash8(key: &Path) -> String {
    let digest = Sha256::digest(key.as_os_str().as_encoded_bytes());
    digest[..4].iter().map(|b| format!("{b:02x}")).collect()
}

/// The stores a config runs with, user first: `(Scope, dir)` for each
/// dir that is set.
pub fn of_config(config: &crate::agent::AgentConfig) -> Vec<(Scope, PathBuf)> {
    [
        (Scope::User, &config.user_memory_dir),
        (Scope::Project, &config.memory_dir),
    ]
    .into_iter()
    .filter_map(|(s, d)| d.clone().map(|d| (s, d)))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ov-stores-{tag}-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::canonicalize(&d).unwrap()
    }

    #[test]
    fn home_prefers_overseer_home_then_home() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| std::ffi::OsString::from(v))
            }
        };
        assert_eq!(
            home_from(env(&[("OVERSEER_HOME", "/o"), ("HOME", "/h")])),
            Some(PathBuf::from("/o"))
        );
        assert_eq!(
            home_from(env(&[("OVERSEER_HOME", ""), ("HOME", "/h")])),
            Some(PathBuf::from("/h/.overseer"))
        );
        assert_eq!(home_from(env(&[])), None);
    }

    #[test]
    fn project_key_walks_up_to_git_dir_or_file_without_git() {
        // A bare `.git` dir/file that no git binary would accept: resolution
        // must succeed from the filesystem alone (no process spawn).
        let root = tmp("repo").join("My Repo");
        let sub = root.join("a/b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        assert_eq!(project_key(&sub), root);

        let wt = tmp("wt");
        std::fs::write(wt.join(".git"), "gitdir: /nowhere\n").unwrap();
        std::fs::create_dir_all(wt.join("src")).unwrap();
        assert_eq!(project_key(&wt.join("src")), wt);

        let plain = tmp("plain");
        assert_eq!(project_key(&plain), plain);
    }

    #[test]
    fn project_store_is_slug_and_hash_of_the_key() {
        let root = tmp("slug").join("My_Repo.Name-With-A-Very-Long-Tail-Beyond-32");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let home = PathBuf::from("/h");
        let store = project_store(&home, &root.join(".git"));
        let dir = store
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap();
        let (slug, hash) = dir.rsplit_once('-').unwrap();
        assert_eq!(slug, "my-repo-name-with-a-very-long-ta");
        assert!(slug.len() <= 32);
        let want = Sha256::digest(root.as_os_str().as_encoded_bytes());
        assert_eq!(
            hash,
            format!(
                "{:02x}{:02x}{:02x}{:02x}",
                want[0], want[1], want[2], want[3]
            )
        );
        assert_eq!(store, home.join("projects").join(dir).join("memory"));
        assert_eq!(user_store(&home), home.join("memory"));
        assert_eq!(super::slug(Path::new("/")), "root");
    }

    #[test]
    fn ensure_creates_private_layers_and_gitignore() {
        let dir = tmp("ensure").join("memory");
        crate::memory::ensure(&dir).unwrap();
        assert!(crate::harden::is_private_dir(&dir));
        for l in crate::memory::Layer::ALL {
            assert!(
                crate::harden::is_private_dir(&dir.join(l.name())),
                "{}",
                l.name()
            );
        }
        assert_eq!(
            std::fs::read_to_string(dir.join(".gitignore")).unwrap(),
            ".index/\n"
        );
        assert!(dir.join("INDEX.md").is_file());
    }
}
