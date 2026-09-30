//! Per-session episode note (decision record §6): a small, engine-authored
//! `episodic/session-<YYYY-MM-DD>-<id8>.md` in the project store, derived
//! only from the session's events so a resumed session rewrites the same
//! bytes it would have written live.

use super::stores::Scope;
use super::Layer;
use crate::event::{Event, EventKind};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

const PROMPT_MAX: usize = 300;
const FILES_MAX: usize = 20;
/// Sessions below both bars (no file change, < 3 steps) leave no episode.
const MIN_STEPS: u32 = 3;

/// A derived episode: store-relative path, index title, full note text.
#[derive(Debug, PartialEq, Eq)]
pub struct Episode {
    pub rel: String,
    pub title: String,
    pub text: String,
}

/// The episode for `events`, or None below the threshold or with no
/// `SessionStart`. The date, `id8` and `valid_from` come from the first
/// `SessionStart`.
pub fn derive(events: &[Event]) -> Option<Episode> {
    let (start_ms, id, cwd, mut model) = events.iter().find_map(|e| match &e.kind {
        EventKind::SessionStart {
            session_id,
            cwd,
            model,
            ..
        } => Some((e.ts_ms, session_id.as_str(), Path::new(cwd), model.as_str())),
        _ => None,
    })?;
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut prompts = Vec::new();
    let mut calls: HashMap<&str, &str> = HashMap::new();
    let mut changed: Vec<String> = Vec::new();
    let (mut bash, mut steps, mut cost, mut outcome) = (0usize, 0u32, 0.0f64, "unfinished");
    for e in events {
        match &e.kind {
            EventKind::UserInput { text } => prompts.push(text.as_str()),
            EventKind::ToolCallStart {
                call_id,
                name,
                input,
            } => {
                if name == "bash" && !input.is_null() {
                    bash += 1;
                }
                if name == "write" || name == "edit" {
                    if let Some(p) = input.get("path").and_then(serde_json::Value::as_str) {
                        calls.insert(call_id, p);
                    }
                }
            }
            EventKind::ToolResult {
                call_id, is_error, ..
            } => {
                if let (false, Some(p)) = (*is_error, calls.remove(call_id.as_str())) {
                    let rel = display_path(Path::new(p), cwd, home.as_deref());
                    if !changed.contains(&rel) {
                        changed.push(rel);
                    }
                }
            }
            EventKind::ModelSwitch { to, .. } => model = to,
            EventKind::RunEnd {
                stop_reason,
                steps: s,
                total_cost_usd,
                ..
            } => {
                steps += s;
                cost = *total_cost_usd;
                outcome = stop_reason;
            }
            _ => {}
        }
    }
    if changed.is_empty() && steps < MIN_STEPS {
        return None;
    }
    let start = super::rfc3339(start_ms / 1000);
    let date = &start[..10];
    let id8 = crate::tools::memory_tool::id8(id);
    let first = super::redact::scrub(prompts.first().map_or("", |p| p.trim()));
    let first = first.as_ref();
    let one_line = |s: &str, n: usize| -> String {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(n)
            .collect()
    };
    let title = format!("Session {date} {id8}: {}", one_line(first, 60));
    let mut files = changed
        .iter()
        .take(FILES_MAX)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if changed.len() > FILES_MAX {
        files.push_str(&format!(" (+{} more)", changed.len() - FILES_MAX));
    }
    if files.is_empty() {
        files = "none".into();
    }
    let text = format!(
        "---\nprovenance: engine\nconfidence: 0.9\nvalid_from: {start}\n---\n# {title}\n\
         Prompt: {}\nLater prompts: {}\nFiles changed: {files}\nBash calls: {bash}\n\
         Outcome: {outcome} after {steps} steps, ${cost:.4}, model {model}\n",
        one_line(first, PROMPT_MAX),
        prompts.len().saturating_sub(1),
    );
    Some(Episode {
        rel: format!("episodic/session-{date}-{id8}.md"),
        title,
        text,
    })
}

/// A changed file as the episode names it: cwd-relative inside the
/// workspace, `~`-relative under `$HOME`, else `…/<basename>` — never an
/// absolute path.
fn display_path(p: &Path, cwd: &Path, home: Option<&Path>) -> String {
    use std::path::Component;
    let tail = || {
        p.file_name()
            .map_or_else(|| "…".into(), |f| format!("…/{}", f.to_string_lossy()))
    };
    let shown = |rel: &Path| {
        if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.display().to_string()
        }
    };
    if p.is_relative() {
        if p.components().any(|c| c == Component::ParentDir) {
            return tail();
        }
        let p = p.strip_prefix(".").unwrap_or(p);
        return shown(p);
    }
    if let Ok(rel) = p.strip_prefix(cwd) {
        return shown(rel);
    }
    let home = home.filter(|h| h.is_absolute() && h.parent().is_some());
    if let Some(rel) = home.and_then(|h| p.strip_prefix(h).ok()) {
        return if rel.as_os_str().is_empty() {
            "~".into()
        } else {
            format!("~/{}", rel.display())
        };
    }
    tail()
}

/// Write (rewrite in place) the session's episode into project store
/// `dir`; its INDEX pointer is appended once. Returns the rel path.
pub fn write(dir: &Path, events: &[Event]) -> std::io::Result<Option<String>> {
    let Some(ep) = derive(events) else {
        return Ok(None);
    };
    crate::harden::ensure_private_dir(&dir.join("episodic"))?;
    std::fs::write(dir.join(&ep.rel), &ep.text)?;
    let index = std::fs::read_to_string(dir.join(super::INDEX_NAME)).unwrap_or_default();
    if !index.lines().any(|l| l.trim_start().starts_with(&ep.rel)) {
        super::append_pointer(dir, &format!("{} — {}", ep.rel, ep.title))?;
    }
    Ok(Some(ep.rel))
}

/// Consolidation input cap, chars (instructions, note list and episodes).
const DISTILL_INPUT_MAX: usize = 12_000;
/// Reply lines the engine will consider; later lines count as invalid.
const DISTILL_LINES_MAX: usize = 5;
/// Distillation ledger: one `rel<TAB>sha256(body)[..12]` line per episode
/// already distilled; an episode is distilled again only when its body
/// hash changes.
pub const DISTILLED: &str = ".index/distilled";

fn body_hash(text: &str) -> String {
    let body = super::parse_meta(text).map_or_else(|_| text.to_string(), |(_, b)| b);
    super::sha_hex(body.as_bytes(), 12)
}

fn read_ledger(project: &Path) -> std::collections::BTreeMap<String, String> {
    std::fs::read_to_string(project.join(DISTILLED))
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(r, h)| (r.to_string(), h.to_string()))
        .collect()
}

/// What one distillation pass did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Distilled {
    pub episodes: usize,
    /// `scope:layer/name.md` of every note written (ADD and SUPERSEDE).
    pub added: Vec<String>,
    pub superseded: usize,
    pub duplicates: usize,
    pub invalid: usize,
}

/// Distil the project store's episodes not yet in [`DISTILLED`] (by body)
/// into at most five validated semantic/procedural notes: one small-model
/// call whose reply lines must be `ADD <semantic|procedural> <name> |
/// <cues> | <text>` or `SUPERSEDE <scope:layer/name.md> -> <new-name> |
/// <text>` (the target qualified as for `forget`).
/// Anything else is skipped and counted; `profile/` is never written.
pub fn distill(
    provider: &dyn crate::provider::Provider,
    model: &str,
    stores: &[(Scope, PathBuf)],
    now: u64,
) -> Result<Distilled, String> {
    let Some(project) = stores
        .iter()
        .find(|(s, _)| *s == Scope::Project)
        .map(|(_, d)| d)
    else {
        return Ok(Distilled::default());
    };
    let mut ledger = read_ledger(project);
    let mut fresh: Vec<(u64, String, String, String)> = std::fs::read_dir(project.join("episodic"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".md") {
                return None;
            }
            let mtime = e.metadata().ok()?.modified().ok()?;
            let secs = mtime.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
            let text = std::fs::read_to_string(e.path()).ok()?;
            let hash = body_hash(&text);
            let rel = format!("episodic/{name}");
            (ledger.get(&rel) != Some(&hash)).then_some((secs, name, text, hash))
        })
        .collect();
    fresh.sort();
    if fresh.is_empty() {
        return Ok(Distilled::default());
    }
    let idx = super::index::Index::build(stores, now);
    let mut existing = String::new();
    for d in idx
        .docs
        .iter()
        .filter(|d| matches!(d.layer(), Some(Layer::Semantic | Layer::Procedural)))
    {
        existing.push_str(&format!("{} — {}\n", d.id(), d.title));
    }
    let mut input = format!(
        "Distil durable facts (semantic) and how-tos (procedural) from these agent \
         session episodes. Reply with at most {DISTILL_LINES_MAX} lines, nothing else, each \
         exactly one of:\nADD <semantic|procedural> <name> | <cues> | <text>\n\
         SUPERSEDE <existing scope:layer/name.md> -> <new-name> | <text>\n\
         Only what a future session would need; no reply lines if nothing is durable.\n\
         == EXISTING NOTES ==\n{existing}== EPISODES ==\n"
    );
    let mut used = Vec::new();
    let mut chars = input.chars().count();
    for (_, name, text, hash) in &fresh {
        let block = format!("### {name}\n{text}\n");
        let n = block.chars().count();
        if chars + n > DISTILL_INPUT_MAX {
            if !used.is_empty() {
                break;
            }
            // The one bounded cut: the first episode always goes in, clipped.
            input.extend(block.chars().take(DISTILL_INPUT_MAX.saturating_sub(chars)));
        } else {
            input.push_str(&block);
        }
        chars += n;
        used.push((name.as_str(), hash.as_str()));
    }
    let msgs = [crate::ir::Message::user_text(input)];
    let req = crate::provider::Request {
        model,
        system: &[],
        tools: &[],
        messages: &msgs,
        max_tokens: 1_024,
        thinking_budget: None,
        effort: Some(crate::provider::Effort::Min),
        cache_breakpoints: false,
        cache_key: None,
    };
    let resp = provider
        .complete(&req)
        .map_err(|e| format!("distill: {e}"))?;
    let reply: String = resp
        .blocks
        .iter()
        .filter_map(|b| match b {
            crate::ir::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    let names: Vec<&str> = used.iter().map(|(n, _)| *n).collect();
    let meta = |cues: &str| {
        let mut m = format!(
            "provenance: consolidate:{}\nconfidence: 0.6\n",
            names.join(",")
        );
        if !cues.is_empty() {
            m.push_str(&format!("cues: {cues}\n"));
        }
        m.push_str(&format!("valid_from: {}\n", super::rfc3339(now)));
        m
    };
    let norm = |s: &str| {
        s.split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };
    let mut out = Distilled {
        episodes: used.len(),
        ..Distilled::default()
    };
    for (i, line) in reply
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .enumerate()
    {
        let applied = if i >= DISTILL_LINES_MAX {
            None
        } else if let Some(rest) = line.strip_prefix("ADD ") {
            parse_add(rest).and_then(|(layer, name, cues, text)| {
                let text = super::redact::scrub(text);
                let cues = super::redact::scrub(&cues);
                let text = text.as_ref();
                let scope = layer.default_scope();
                let dir = stores.iter().find(|(s, _)| *s == scope).map(|(_, d)| d)?;
                if idx
                    .docs
                    .iter()
                    .any(|d| d.scope == scope && norm(&d.body) == norm(text))
                {
                    out.duplicates += 1;
                    return Some(());
                }
                let rel = super::add_note(dir, layer, &name, &meta(&cues), text).ok()?;
                out.added.push(format!("{}:{rel}", scope.name()));
                Some(())
            })
        } else if let Some(rest) = line.strip_prefix("SUPERSEDE ") {
            parse_supersede(rest).and_then(|(old, new, text)| {
                let text = super::redact::scrub(text);
                let text = text.as_ref();
                let doc = idx.resolve_qualified(old).ok()?;
                let layer = doc
                    .layer()
                    .filter(|l| matches!(l, Layer::Semantic | Layer::Procedural))?;
                let dir = stores
                    .iter()
                    .find(|(s, _)| *s == doc.scope)
                    .map(|(_, d)| d)?;
                let rel = super::add_note(dir, layer, &new, &meta(""), text).ok()?;
                use std::io::Write;
                let old_text = std::fs::read_to_string(&doc.path).ok()?;
                let sep = if old_text.ends_with('\n') { "" } else { "\n" };
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(&doc.path)
                    .and_then(|mut f| f.write_all(format!("{sep}superseded_by {rel}\n").as_bytes()))
                    .ok()?;
                out.added.push(format!("{}:{rel}", doc.scope.name()));
                out.superseded += 1;
                Some(())
            })
        } else {
            None
        };
        if applied.is_none() {
            out.invalid += 1;
        }
    }
    if !used.is_empty() {
        for (name, hash) in &used {
            ledger.insert(format!("episodic/{name}"), (*hash).to_string());
        }
        let lines: String = ledger.iter().map(|(r, h)| format!("{r}\t{h}\n")).collect();
        crate::harden::ensure_private_dir(&project.join(".index")).map_err(|e| e.to_string())?;
        std::fs::write(project.join(DISTILLED), lines).map_err(|e| e.to_string())?;
    }
    Ok(out)
}

/// `<semantic|procedural> <name> | <cues> | <text>` → (layer, slug, cues, text).
fn parse_add(rest: &str) -> Option<(Layer, String, String, &str)> {
    let (head, tail) = rest.split_once('|')?;
    let (cues, text) = tail.split_once('|')?;
    let (layer, name) = head.trim().split_once(' ')?;
    let layer = Layer::parse(layer).filter(|l| matches!(l, Layer::Semantic | Layer::Procedural))?;
    let name = slug(name)?;
    let cues = cues
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    let text = text.trim();
    (!text.is_empty()).then_some((layer, name, cues, text))
}

/// `<scope:layer/name.md> -> <new-name> | <text>` → (existing, new slug, text).
fn parse_supersede(rest: &str) -> Option<(&str, String, &str)> {
    let (head, text) = rest.split_once('|')?;
    let (old, new) = head.split_once("->")?;
    let (old, text) = (old.trim(), text.trim());
    let new = slug(new)?;
    (!old.is_empty() && !text.is_empty()).then_some((old, new, text))
}

/// A model-supplied note name as a store slug: exactly one token, which
/// must already be a slug (a `.md` suffix is tolerated).
fn slug(name: &str) -> Option<String> {
    let name = name.trim();
    let name = name.strip_suffix(".md").unwrap_or(name);
    let s = super::stores::slugify(name, 48);
    (!s.is_empty() && s == name).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(id: u64, kind: EventKind) -> Event {
        Event {
            id,
            parent_id: None,
            ts_ms: 1_790_000_000_000 + id,
            prev_hash: 0,
            hash: 0,
            kind,
        }
    }

    fn session(extra: Vec<EventKind>) -> Vec<Event> {
        let mut kinds = vec![
            EventKind::SessionStart {
                session_id: "1790000000123".into(),
                cwd: "/w".into(),
                model: "m1".into(),
                harness_version: "0".into(),
                parent: None,
            },
            EventKind::UserInput {
                text: "fix   the\nbuild".into(),
            },
        ];
        kinds.extend(extra);
        kinds
            .into_iter()
            .enumerate()
            .map(|(i, k)| ev(i as u64, k))
            .collect()
    }

    fn call(id: &str, name: &str, input: serde_json::Value, err: bool) -> [EventKind; 2] {
        [
            EventKind::ToolCallStart {
                call_id: id.into(),
                name: name.into(),
                input,
            },
            EventKind::ToolResult {
                call_id: id.into(),
                name: name.into(),
                content: String::new(),
                is_error: err,
                raw_bytes: 0,
                spilled_to: None,
                denied: false,
            },
        ]
    }

    fn run_end(steps: u32) -> EventKind {
        EventKind::RunEnd {
            stop_reason: "completed".into(),
            steps,
            total_cost_usd: 0.25,
            subagent_cost_usd: 0.0,
            cache: Default::default(),
        }
    }

    #[test]
    fn threshold_needs_a_change_or_three_steps() {
        assert_eq!(derive(&session(vec![run_end(2)])), None);
        assert!(derive(&session(vec![run_end(1), run_end(2)])).is_some());
        let mut kinds: Vec<EventKind> = call("a", "write", json!({"path": "/w/x.rs"}), true).into();
        kinds.push(run_end(1));
        assert_eq!(
            derive(&session(kinds)),
            None,
            "a failed write changes nothing"
        );
        assert_eq!(derive(&[]), None);
    }

    #[test]
    fn derivation_is_deterministic_and_complete() {
        let mut kinds: Vec<EventKind> = Vec::new();
        kinds.extend(call("a", "write", json!({"path": "/w/src/a.rs"}), false));
        kinds.extend(call("b", "edit", json!({"path": "src/a.rs"}), false));
        kinds.extend(call("c", "bash", json!({"command": "ls"}), false));
        kinds.extend(call("d", "bash", serde_json::Value::Null, false));
        kinds.push(EventKind::UserInput {
            text: "again".into(),
        });
        kinds.push(EventKind::ModelSwitch {
            from: "m1".into(),
            to: "m2".into(),
            price_in: 0.0,
            price_out: 0.0,
        });
        kinds.push(run_end(2));
        let events = session(kinds);
        let ep = derive(&events).unwrap();
        assert_eq!(ep, derive(&events).unwrap());
        assert_eq!(ep.rel, "episodic/session-2026-09-21-00000123.md");
        assert_eq!(ep.title, "Session 2026-09-21 00000123: fix the build");
        assert_eq!(
            ep.text,
            "---\nprovenance: engine\nconfidence: 0.9\nvalid_from: 2026-09-21T14:13:20Z\n---\n\
             # Session 2026-09-21 00000123: fix the build\nPrompt: fix the build\n\
             Later prompts: 1\nFiles changed: src/a.rs\nBash calls: 1\n\
             Outcome: completed after 2 steps, $0.2500, model m2\n"
        );
    }

    #[test]
    fn episode_paths_are_never_absolute() {
        let (cwd, home) = (Path::new("/w/proj"), Some(Path::new("/h/me")));
        for (p, want) in [
            ("/w/proj/src/a.rs", "src/a.rs"),
            ("src/a.rs", "src/a.rs"),
            ("./src/a.rs", "src/a.rs"),
            ("/h/me/.config/x.toml", "~/.config/x.toml"),
            ("/etc/hosts", "…/hosts"),
            ("../other/b.rs", "…/b.rs"),
            ("/w/project2/c.rs", "…/c.rs"),
        ] {
            assert_eq!(display_path(Path::new(p), cwd, home), want, "{p}");
        }
        assert_eq!(
            display_path(Path::new("/h/me/x"), cwd, Some(Path::new("/"))),
            "…/x",
            "a root HOME is no anchor"
        );
        let mut kinds: Vec<EventKind> = Vec::new();
        kinds.extend(call(
            "a",
            "write",
            json!({"path": "/opt/elsewhere/notes.txt"}),
            false,
        ));
        kinds.push(run_end(1));
        let ep = derive(&session(kinds)).unwrap();
        assert!(
            ep.text.contains("Files changed: …/notes.txt\n"),
            "{}",
            ep.text
        );
        assert!(!ep.text.contains("/opt/"), "{}", ep.text);
    }

    #[test]
    fn episode_prompt_is_redacted_before_truncation() {
        let mut events = session(vec![run_end(3)]);
        events[1].kind = EventKind::UserInput {
            text: "deploy with sk-abcdefghijklmnopqrstuvwxyz0123 please".into(),
        };
        let ep = derive(&events).unwrap();
        assert!(!ep.text.contains("sk-abc"), "{}", ep.text);
        assert!(ep
            .text
            .contains("Prompt: deploy with [redacted:api-key] please"));
        assert!(!ep.title.contains("sk-abc"), "{}", ep.title);
    }

    #[test]
    fn write_rewrites_in_place_and_points_once() {
        let dir = std::env::temp_dir().join(format!("ov-episode-{}", uuid::Uuid::now_v7()));
        crate::memory::ensure(&dir).unwrap();
        let events = session(vec![run_end(3)]);
        let rel = write(&dir, &events).unwrap().unwrap();
        let events = session(vec![run_end(3), run_end(4)]);
        assert_eq!(write(&dir, &events).unwrap().unwrap(), rel);
        let text = std::fs::read_to_string(dir.join(&rel)).unwrap();
        assert!(text.contains("after 7 steps"));
        let index = std::fs::read_to_string(dir.join(crate::memory::INDEX_NAME)).unwrap();
        assert_eq!(index.matches(&rel).count(), 1);
    }

    struct Reply {
        text: String,
        seen: std::sync::Mutex<Vec<String>>,
    }

    impl crate::provider::Provider for Reply {
        fn complete(
            &self,
            req: &crate::provider::Request,
        ) -> Result<crate::provider::Response, crate::provider::ProviderError> {
            self.seen.lock().unwrap().push(req.messages[0].text());
            Ok(crate::provider::Response {
                blocks: vec![crate::ir::Block::Text {
                    text: self.text.clone(),
                }],
                stop_reason: crate::provider::StopReason::EndTurn,
                usage: crate::ir::Usage::default(),
                request_bytes: 0,
                latency_ms: 0,
            })
        }
        fn name(&self) -> &'static str {
            "reply"
        }
    }

    #[test]
    fn distill_validates_every_line_and_runs_once_per_episode() {
        const NOW: u64 = 1_790_000_000;
        let root = std::env::temp_dir().join(format!("ov-distill-{}", uuid::Uuid::now_v7()));
        let stores = vec![
            (Scope::User, root.join("user")),
            (Scope::Project, root.join("project")),
        ];
        for (_, d) in &stores {
            super::super::ensure(d).unwrap();
        }
        let project = &stores[1].1;
        std::fs::write(project.join("semantic/db.md"), "# DB\npostgres 15\n").unwrap();
        super::super::append_pointer(project, "semantic/db.md — DB").unwrap();
        std::fs::write(
            project.join("episodic/session-a.md"),
            "# Session a\nupgraded db\n",
        )
        .unwrap();
        let reply = Reply {
            text: "ADD semantic ci-cache | ci, cache | CI caches target/ per branch\n\
                   ADD profile me | x | I am the owner\n\
                   ADD semantic Bad Name | x | spaces are not a slug\n\
                   SUPERSEDE semantic/db.md -> db-16 | postgres 16 since the upgrade\n\
                   SUPERSEDE db -> x | unqualified target\n\
                   ADD procedural release | ship | tag then push\n\
                   garbage"
                .into(),
            seen: Default::default(),
        };
        let d = distill(&reply, "small", &stores, NOW).unwrap();
        assert_eq!(
            d,
            Distilled {
                episodes: 1,
                added: vec![
                    "project:semantic/ci-cache.md".into(),
                    "project:semantic/db-16.md".into()
                ],
                superseded: 1,
                duplicates: 0,
                invalid: 5,
            },
            "profile, bad slug, unqualified target and lines past the fifth are skipped"
        );
        let input = reply.seen.lock().unwrap()[0].clone();
        assert!(
            input.chars().count() <= DISTILL_INPUT_MAX
                && input.contains("project:semantic/db.md — DB")
        );
        let add = std::fs::read_to_string(project.join("semantic/ci-cache.md")).unwrap();
        assert!(
            add.starts_with(
                "---\nprovenance: consolidate:session-a.md\nconfidence: 0.6\ncues: ci, cache\n"
            ),
            "{add}"
        );
        let old = std::fs::read_to_string(project.join("semantic/db.md")).unwrap();
        assert_eq!(
            old, "# DB\npostgres 15\nsuperseded_by semantic/db-16.md\n",
            "ADD-only trailer"
        );
        assert!(!stores[0].1.join("profile/me.md").exists());
        // No new episodes → no model call.
        assert_eq!(
            distill(&reply, "small", &stores, NOW).unwrap(),
            Distilled::default()
        );
        assert_eq!(reply.seen.lock().unwrap().len(), 1);
        // A newer episode is distilled; a fact already stored is a duplicate.
        let b = project.join("episodic/session-b.md");
        std::fs::write(&b, "# Session b\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::options()
            .write(true)
            .open(&b)
            .unwrap()
            .set_modified(later)
            .unwrap();
        let again = Reply {
            text: "ADD semantic cache2 | ci | CI caches   target/ per branch".into(),
            seen: Default::default(),
        };
        let d = distill(&again, "small", &stores, NOW).unwrap();
        assert_eq!((d.episodes, d.duplicates, d.added.len()), (1, 1, 0));
        assert!(
            again.seen.lock().unwrap()[0].contains("### session-b.md")
                && !again.seen.lock().unwrap()[0].contains("### session-a.md")
        );
    }

    /// Resume rewrites the episode with identical bytes (new mtime): the
    /// body hash is already in the ledger, so consolidate does not distil
    /// it again; a changed body is distilled once more.
    #[test]
    fn distill_once_per_episode_body_across_rewrites() {
        let root = std::env::temp_dir().join(format!("ov-distill-l-{}", uuid::Uuid::now_v7()));
        let stores = vec![
            (Scope::User, root.join("user")),
            (Scope::Project, root.join("project")),
        ];
        for (_, d) in &stores {
            super::super::ensure(d).unwrap();
        }
        let project = &stores[1].1;
        let reply = Reply {
            text: "---INDEX---\n# Memory Index\n---INDEX---".into(),
            seen: Default::default(),
        };
        let distills = || {
            reply
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.starts_with("Distil durable facts"))
                .count()
        };
        let live = session(vec![run_end(3)]);
        let rel = write(project, &live).unwrap().unwrap();
        crate::memory::consolidate_stores(&reply, "small", &stores, 1_790_000_000).unwrap();
        assert_eq!(distills(), 1);
        let ledger = std::fs::read_to_string(project.join(DISTILLED)).unwrap();
        let hash = body_hash(&std::fs::read_to_string(project.join(&rel)).unwrap());
        assert_eq!(ledger, format!("{rel}\t{hash}\n"));
        assert_eq!(hash.len(), 12);
        // Resume: same events → same bytes, fresh mtime.
        std::thread::sleep(std::time::Duration::from_millis(1_100));
        write(project, &live).unwrap();
        crate::memory::consolidate_stores(&reply, "small", &stores, 1_790_000_100).unwrap();
        assert_eq!(distills(), 1, "unchanged body is never distilled twice");
        // The session continued: the body changed, so it is distilled once.
        write(project, &session(vec![run_end(3), run_end(2)])).unwrap();
        crate::memory::consolidate_stores(&reply, "small", &stores, 1_790_000_200).unwrap();
        crate::memory::consolidate_stores(&reply, "small", &stores, 1_790_000_300).unwrap();
        assert_eq!(distills(), 2);
    }

    #[test]
    fn distilled_text_is_redacted() {
        let root = std::env::temp_dir().join(format!("ov-distill-r-{}", uuid::Uuid::now_v7()));
        let stores = vec![
            (Scope::User, root.join("user")),
            (Scope::Project, root.join("project")),
        ];
        for (_, d) in &stores {
            super::super::ensure(d).unwrap();
        }
        let project = &stores[1].1;
        std::fs::write(
            project.join("semantic/db.md"),
            "# DB
postgres 15
",
        )
        .unwrap();
        super::super::append_pointer(project, "semantic/db.md — DB").unwrap();
        std::fs::write(
            project.join("episodic/session-a.md"),
            "# Session a
",
        )
        .unwrap();
        let reply = Reply {
            text: "ADD semantic ci | ci | CI uses token: abcdefgh12345678
\
                   SUPERSEDE project:semantic/db.md -> db-16 | dsn secret=pg-pass-0123456789"
                .into(),
            seen: Default::default(),
        };
        let d = distill(&reply, "small", &stores, 1_790_000_000).unwrap();
        assert_eq!(d.added.len(), 2, "{d:?}");
        let ci = std::fs::read_to_string(project.join("semantic/ci.md")).unwrap();
        let db = std::fs::read_to_string(project.join("semantic/db-16.md")).unwrap();
        assert!(ci.contains("CI uses [redacted:secret]"), "{ci}");
        assert!(
            db.contains("dsn [redacted:secret]") && !db.contains("pg-pass"),
            "{db}"
        );
    }
}
