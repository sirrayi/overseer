//! Engine-injected memory text (decision record §5): recall on user input
//! and prospective reminders. Each fires as a logged `MemoryNotice` whose
//! `text` rehydrates like a `Nudge`, so a resumed view is byte-identical.
// DEFERRED(owner): recurring triggers (every:) and idle-time firing via the gateway daemon — gate: v2 in use

use super::index::{self, Doc, Index};
use super::Layer;
use std::collections::HashSet;
use std::path::Path;

/// Recall admits a note only when it matches at least this many distinct
/// non-stopword query terms (1 for a one-term query): a single shared
/// word between a prompt and a note is noise, not relevance.
pub const RECALL_MIN_TERMS: usize = 2;
/// …and its matched terms carry at least this share of the query's total
/// idf: the rare, content-bearing words must be covered, not just the
/// common ones. Tuned conservative — an unsolicited notice costs context
/// on every turn, a missed one costs a `memory search`.
pub const RECALL_MIN_COVERAGE: f64 = 0.35;
/// At most this many notes per recall block.
pub const RECALL_TOP: usize = 3;
const RECALL_CAP: usize = 1_200;
const RECALL_SNIPPET: usize = 240;
const REMINDER_BODY: usize = 400;
pub const RECALL_HEADER: &str = "[memory] possibly relevant notes (verify before relying):";

/// One injected block: `kind` is `recall` or `reminder`; `notes` are the
/// `scope:rel` ids it names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: &'static str,
    pub notes: Vec<String>,
    pub text: String,
}

/// A prospective note's `trigger:`.
#[derive(Debug, Clone)]
pub enum Trigger {
    /// `at:<rfc3339>` — due once now ≥ t (checked on user input).
    At(u64),
    /// `kw:<a,b>` — any term appears as a token of the user input.
    Kw(Vec<String>),
    /// `path:<glob>` — a successful read/write/edit touches a matching
    /// cwd-relative path.
    Path(globset::GlobMatcher),
}

impl Trigger {
    pub fn parse(s: &str) -> Result<Trigger, String> {
        let s = s.trim();
        if let Some(t) = s.strip_prefix("at:") {
            return super::rfc3339_epoch(t.trim())
                .map(Trigger::At)
                .ok_or_else(|| format!("trigger `{s}`: at: needs an RFC3339 UTC time"));
        }
        if let Some(list) = s.strip_prefix("kw:") {
            let mut terms = Vec::new();
            for raw in list.split(',').map(str::trim).filter(|t| !t.is_empty()) {
                match index::raw_terms(raw).as_slice() {
                    [one] => terms.push(one.clone()),
                    _ => return Err(format!("trigger `{s}`: `{raw}` is not a single word")),
                }
            }
            if terms.is_empty() {
                return Err(format!("trigger `{s}`: kw: needs at least one word"));
            }
            return Ok(Trigger::Kw(terms));
        }
        if let Some(glob) = s.strip_prefix("path:") {
            return globset::GlobBuilder::new(glob.trim())
                .literal_separator(true)
                .build()
                .map(|g| Trigger::Path(g.compile_matcher()))
                .map_err(|e| format!("trigger `{s}`: {e}"));
        }
        Err(format!(
            "trigger `{s}`: expected at:<rfc3339>, kw:<a,b> or path:<glob>"
        ))
    }
}

/// Provenance that marks untrusted or external origin.
fn unverified(provenance: &str) -> bool {
    ["tainted", "untrusted", "external"]
        .iter()
        .any(|m| provenance.contains(m))
}

/// Recall for one user input: the top [`RECALL_TOP`] fused hits that pass
/// both thresholds and are not in `seen`, rendered into one block of at
/// most 1,200 chars. Prospective notes never recall (they remind).
pub fn recall(idx: &Index, input: &str, seen: &HashSet<String>, now: u64) -> Option<Notice> {
    let q = index::query_terms(input);
    if q.is_empty() {
        return None;
    }
    let need = RECALL_MIN_TERMS.min(q.len());
    let mut text = RECALL_HEADER.to_string();
    let mut notes = Vec::new();
    for hit in idx.search(input, now) {
        if notes.len() == RECALL_TOP {
            break;
        }
        let doc = &idx.docs[hit.doc];
        if hit.matched < need
            || hit.coverage < RECALL_MIN_COVERAGE
            || doc.layer() == Some(Layer::Prospective)
            || engine_written(&doc.meta.provenance)
            || seen.contains(&doc.id())
        {
            continue;
        }
        let snip = index::snippet(doc, input, RECALL_SNIPPET).replace('"', "'");
        let label = if unverified(&doc.meta.provenance) {
            " (unverified origin)"
        } else {
            ""
        };
        let line = format!("\n- {} — \"{snip}\"{label}", doc.id());
        if text.len() + line.len() > RECALL_CAP {
            break;
        }
        text.push_str(&line);
        notes.push(doc.id());
    }
    (!notes.is_empty()).then_some(Notice {
        kind: "recall",
        notes,
        text,
    })
}

/// Engine-authored notes (session episodes) stay out of recall; they
/// remain searchable and gettable.
fn engine_written(provenance: &str) -> bool {
    provenance.split_whitespace().next() == Some("engine")
}

/// What a reminder is checked against.
pub enum Event<'a> {
    /// A user input at clock `now` — `at:` and `kw:` triggers.
    Input(&'a str),
    /// A cwd-relative path a successful read/write/edit touched.
    Path(&'a Path),
}

/// Unfired prospective notes whose trigger matches `ev` at `now`, in doc
/// order. Notes with an unparsable trigger never fire.
pub fn due(idx: &Index, ev: &Event, now: u64) -> Vec<usize> {
    let tokens: HashSet<String> = match ev {
        Event::Input(text) => index::raw_terms(text).into_iter().collect(),
        Event::Path(_) => HashSet::new(),
    };
    idx.docs
        .iter()
        .enumerate()
        .filter(|(_, d)| d.layer() == Some(Layer::Prospective) && d.meta.fired.is_none())
        .filter(|(_, d)| {
            let Some(trigger) = &d.trigger else {
                return false;
            };
            match (trigger, ev) {
                (Trigger::At(t), Event::Input(_)) => now >= *t,
                (Trigger::Kw(terms), Event::Input(_)) => terms.iter().any(|t| tokens.contains(t)),
                (Trigger::Path(glob), Event::Path(p)) => glob.is_match(p),
                _ => false,
            }
        })
        .map(|(i, _)| i)
        .collect()
}

/// Fire one due reminder, exactly once across processes: claim it with
/// `create_new` on `<store>/.index/fired/<sha256(rel)[..16]>`, then stamp
/// `fired: <now>` into the note (the engine's only frontmatter write) and
/// render the notice. `None` when another process already claimed it.
pub fn fire(doc: &mut Doc, now: u64) -> std::io::Result<Option<Notice>> {
    let stamp = super::rfc3339(now);
    let depth = doc.rel.split('/').count();
    let store = doc
        .path
        .ancestors()
        .nth(depth)
        .ok_or_else(|| std::io::Error::other("note outside its store"))?;
    let claims = store.join(".index").join("fired");
    crate::harden::ensure_private_dir(&claims)?;
    let claim = claims.join(super::sha_hex(doc.rel.as_bytes(), 16));
    let won = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&claim);
    doc.meta.fired = Some(stamp.clone());
    match won {
        Ok(mut f) => {
            use std::io::Write;
            f.write_all(format!("{}\t{stamp}\n", doc.rel).as_bytes())?;
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(None),
        Err(e) => return Err(e),
    }
    let text = std::fs::read_to_string(&doc.path)?;
    std::fs::write(&doc.path, super::set_meta_key(&text, "fired", &stamp))?;
    let body: String = doc.body.trim().chars().take(REMINDER_BODY).collect();
    Ok(Some(Notice {
        kind: "reminder",
        notes: vec![doc.id()],
        text: format!("[reminder] {body} (memory: {})", doc.rel),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Scope;
    use std::path::PathBuf;

    const NOW: u64 = 1_900_000_000;

    fn store() -> PathBuf {
        let d = std::env::temp_dir().join(format!("ov-notice-{}", uuid::Uuid::now_v7()));
        crate::memory::ensure(&d).unwrap();
        d
    }

    fn build(dir: &Path) -> Index {
        Index::build(&[(Scope::Project, dir.to_path_buf())], NOW)
    }

    #[test]
    fn recall_thresholds_and_dedup() {
        let dir = store();
        std::fs::write(
            dir.join("semantic/deploy.md"),
            "# Deploy\nstaging deploy uses the blue green switch\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantic/web.md"),
            "---\nprovenance: web tainted:fetch\n---\n# Web\nstaging deploy notes from a blog\n",
        )
        .unwrap();
        std::fs::write(dir.join("semantic/misc.md"), "# Misc\nstaging area only\n").unwrap();
        let idx = build(&dir);
        let mut seen = HashSet::new();
        // One shared term of a multi-term query: below RECALL_MIN_TERMS.
        assert_eq!(recall(&idx, "staging lunch menu", &seen, NOW), None);
        // Two terms but the rare ones uncovered: below the coverage bar.
        assert_eq!(
            recall(
                &idx,
                "staging deploy zebra quokka narwhal axolotl",
                &seen,
                NOW
            ),
            None
        );
        let n = recall(&idx, "how does the staging deploy", &seen, NOW).unwrap();
        assert_eq!(n.kind, "recall");
        assert!(n.text.starts_with(RECALL_HEADER));
        assert_eq!(
            n.notes.len(),
            2,
            "misc matches one term only: {:?}",
            n.notes
        );
        assert!(n.text.contains(
            "- project:semantic/web.md — \"staging deploy notes from a blog\" (unverified origin)"
        ));
        assert!(n
            .text
            .contains("project:semantic/deploy.md — \"staging deploy uses"));
        assert!(!n
            .text
            .contains("deploy.md — \"staging deploy uses the blue green switch\" (unverified"));
        seen.extend(n.notes);
        assert_eq!(
            recall(&idx, "how does the staging deploy", &seen, NOW),
            None
        );
        // A one-term query needs one term.
        let dir = store();
        std::fs::write(dir.join("semantic/k.md"), "# K\nkubernetes cluster\n").unwrap();
        assert!(recall(&build(&dir), "kubernetes?", &HashSet::new(), NOW).is_some());
    }

    #[test]
    fn engine_episodes_skip_recall_but_stay_searchable() {
        let dir = store();
        std::fs::write(
            dir.join("episodic/session-a.md"),
            "---\nprovenance: engine\nconfidence: 0.9\n---\n# Session a\nstaging deploy rollout\n",
        )
        .unwrap();
        let idx = build(&dir);
        assert_eq!(
            recall(&idx, "the staging deploy", &HashSet::new(), NOW),
            None
        );
        let hits = idx.search("staging deploy", NOW);
        assert_eq!(hits.len(), 1);
        assert_eq!(idx.docs[hits[0].doc].id(), "project:episodic/session-a.md");
        assert!(idx.find("project:episodic/session-a.md").is_some());
    }

    #[test]
    fn recall_block_is_capped() {
        let dir = store();
        let long = "alpha beta ".repeat(60);
        for i in 0..5 {
            std::fs::write(
                dir.join(format!("semantic/n{i}.md")),
                format!("# N\n{long}\n"),
            )
            .unwrap();
        }
        let n = recall(&build(&dir), "alpha beta", &HashSet::new(), NOW).unwrap();
        assert!(n.text.len() <= RECALL_CAP);
        assert_eq!(n.notes.len(), RECALL_TOP);
        assert!(n.text.lines().skip(1).all(|l| l.chars().count() < 240 + 60));
    }

    #[test]
    fn triggers_parse_strictly() {
        assert!(matches!(
            Trigger::parse("at:2030-01-01T00:00:00Z"),
            Ok(Trigger::At(_))
        ));
        assert!(
            matches!(Trigger::parse("kw: Deploys , prod"), Ok(Trigger::Kw(t)) if t == ["deploy", "prod"])
        );
        assert!(matches!(
            Trigger::parse("path:src/*.rs"),
            Ok(Trigger::Path(_))
        ));
        for bad in [
            "at:tomorrow",
            "kw:",
            "kw:two words",
            "path:[",
            "every:1d",
            "",
        ] {
            assert!(Trigger::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn each_trigger_fires_exactly_once() {
        let dir = store();
        let note = |name: &str, trigger: &str, body: &str| {
            std::fs::write(
                dir.join(format!("prospective/{name}.md")),
                format!("---\ntrigger: {trigger}\n---\n{body}\n"),
            )
            .unwrap();
        };
        note("at", "at:2031-01-01T00:00:00Z", "renew the cert");
        note("kw", "kw:release", "bump the changelog");
        note("path", "path:src/*.rs", "run clippy after touching src");
        note("bad", "every:1d", "never");
        let mut idx = build(&dir);
        let at = rfc("2031-01-01T00:00:00Z");
        assert!(due(&idx, &Event::Input("hello"), NOW).is_empty());
        assert!(
            due(&idx, &Event::Path(Path::new("src/a/b.rs")), NOW).is_empty(),
            "literal separator"
        );
        let mut fired = Vec::new();
        for (ev, clock) in [
            (Event::Input("cut a Release today"), NOW),
            (Event::Input("anything"), at),
            (Event::Path(Path::new("src/main.rs")), NOW),
        ] {
            let d = due(&idx, &ev, clock);
            assert_eq!(d.len(), 1);
            let n = fire(&mut idx.docs[d[0]], clock).unwrap().unwrap();
            fired.push(n.text);
            assert!(due(&idx, &ev, clock).is_empty(), "fires once in-session");
        }
        assert_eq!(
            fired[0],
            "[reminder] bump the changelog (memory: prospective/kw.md)"
        );
        assert!(fired[1].starts_with("[reminder] renew the cert"));
        assert!(fired[2].ends_with("(memory: prospective/path.md)"));
        // `fired:` is on disk: a fresh index (next session) stays quiet.
        let idx = Index::build(&[(Scope::Project, dir.clone())], at);
        for ev in [
            Event::Input("release"),
            Event::Path(Path::new("src/main.rs")),
        ] {
            assert!(due(&idx, &ev, at).is_empty());
        }
        let text = std::fs::read_to_string(dir.join("prospective/kw.md")).unwrap();
        assert!(
            text.starts_with("---\ntrigger: kw:release\nfired: 2030-"),
            "{text}"
        );
        assert!(text.ends_with("bump the changelog\n"));
    }

    /// Two sessions over one store each hold their own index (nothing
    /// shared but the files): only the claim winner emits the reminder.
    #[test]
    fn fire_claims_once_across_indexes() {
        let dir = store();
        std::fs::write(
            dir.join("prospective/kw.md"),
            "---\ntrigger: kw:release\n---\nbump the changelog\n",
        )
        .unwrap();
        let mut indexes: Vec<Index> = (0..8).map(|_| build(&dir)).collect();
        let ev = Event::Input("cut a release");
        let won = std::thread::scope(|s| {
            let handles: Vec<_> = indexes
                .iter_mut()
                .map(|idx| {
                    let ev = &ev;
                    s.spawn(move || {
                        let d = due(idx, ev, NOW);
                        assert_eq!(d.len(), 1);
                        let n = fire(&mut idx.docs[d[0]], NOW).unwrap();
                        assert!(due(idx, ev, NOW).is_empty(), "quiet after a lost claim too");
                        n
                    })
                })
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().unwrap())
                .count()
        });
        assert_eq!(won, 1);
        let claim = dir
            .join(".index/fired")
            .join(crate::memory::sha_hex(b"prospective/kw.md", 16));
        assert!(claim.is_file());
    }

    /// Real processes: the test binary re-runs itself as claimants.
    #[test]
    fn fire_claims_once_across_processes() {
        let dir = store();
        std::fs::write(
            dir.join("prospective/kw.md"),
            "---\ntrigger: kw:release\n---\nbump the changelog\n",
        )
        .unwrap();
        let exe = std::env::current_exe().unwrap();
        let children: Vec<_> = (0..4)
            .map(|_| {
                std::process::Command::new(&exe)
                    .args([
                        "--exact",
                        "memory::notice::tests::fire_claim_child",
                        "--ignored",
                        "--nocapture",
                        "--test-threads=1",
                    ])
                    .env("OV_FIRE_STORE", &dir)
                    .stdout(std::process::Stdio::piped())
                    .spawn()
                    .unwrap()
            })
            .collect();
        let fired: usize = children
            .into_iter()
            .map(|c| {
                let out = c.wait_with_output().unwrap();
                assert!(out.status.success());
                String::from_utf8_lossy(&out.stdout)
                    .matches("CLAIM:fired")
                    .count()
            })
            .sum();
        assert_eq!(fired, 1);
    }

    #[test]
    #[ignore = "child process of fire_claims_once_across_processes"]
    fn fire_claim_child() {
        let Some(dir) = std::env::var_os("OV_FIRE_STORE") else {
            return;
        };
        let mut idx = build(Path::new(&dir));
        let d = due(&idx, &Event::Input("release"), NOW);
        let got = d
            .first()
            .map(|i| fire(&mut idx.docs[*i], NOW).unwrap().is_some());
        println!("CLAIM:{}", if got == Some(true) { "fired" } else { "lost" });
    }

    fn rfc(s: &str) -> u64 {
        crate::memory::rfc3339_epoch(s).unwrap()
    }
}
