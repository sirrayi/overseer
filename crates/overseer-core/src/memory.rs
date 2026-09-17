//! File-based memory v1 (playbook Ch.3 §9.4).
//!
//! `/memory/` is a directory of topic files the agent edits with ordinary
//! file tools — no special memory tool (the playbook's minimal option).
//! `INDEX.md` is a ≤25KB file of one-line pointers, injected at the *end of
//! the static prompt region* every turn: the index is always in context,
//! the topic files are read on demand (progressive disclosure).
//!
//! The dir is git-versioned (Letta MemFS): free history, diffs, rollback.
//! Commits are engine-made at turn boundaries, not model actions.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const INDEX_NAME: &str = "INDEX.md";
/// Playbook cap: the index is a pointer table, not a document store.
pub const INDEX_CAP: usize = 25_000;

const SEED_INDEX: &str = "# Memory Index\n\n\
    One line per topic file: `name.md — what it's about`. \
    Keep this index small; details live in the files.\n";

/// Sensitivity tier of one memory entry (P6-1 file-convention header).
/// Default is Personal: memory is about the user until marked otherwise.
/// Drives the P6-2 subagent filter (Secret entries stay out of the
/// quarantined view).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Sensitivity {
    Public,
    #[default]
    Personal,
    Secret,
}

impl Sensitivity {
    pub fn as_str(self) -> &'static str {
        match self {
            Sensitivity::Public => "public",
            Sensitivity::Personal => "personal",
            Sensitivity::Secret => "secret",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "public" => Ok(Sensitivity::Public),
            "personal" => Ok(Sensitivity::Personal),
            "secret" => Ok(Sensitivity::Secret),
            other => Err(format!(
                "memory: bad sensitivity `{other}` — want public|personal|secret"
            )),
        }
    }
}

/// File-convention header for one memory topic file (P6-1): frontmatter
/// between `---` lines carrying provenance, a 0..1 confidence, an
/// optional RFC3339 validity window, and a sensitivity tier. Files
/// without frontmatter read as the unvetted default.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryMeta {
    pub provenance: String,
    pub confidence: f64,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub sensitivity: Sensitivity,
}

impl Default for EntryMeta {
    fn default() -> Self {
        EntryMeta {
            provenance: String::new(),
            confidence: 0.5,
            valid_from: None,
            valid_to: None,
            sensitivity: Sensitivity::Personal,
        }
    }
}

/// Split `text` into `(meta, body)`. No frontmatter → default meta and
/// the whole text as body; malformed frontmatter → Err naming the fault.
/// Range/date/sensitivity validation runs through `validate_meta` so
/// parse and direct construction share one gate.
pub fn parse_meta(text: &str) -> Result<(EntryMeta, String), String> {
    let all: Vec<&str> = text.lines().collect();
    if all.first().map(|l| l.trim()) != Some("---") {
        return Ok((EntryMeta::default(), text.to_string()));
    }
    let mut close = None;
    for (i, l) in all.iter().enumerate().skip(1) {
        if l.trim() == "---" {
            close = Some(i);
            break;
        }
    }
    let Some(end) = close else {
        return Err("memory: unterminated frontmatter — missing closing `---`".into());
    };
    let mut meta = EntryMeta::default();
    for line in &all[1..end] {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (k, v) = line.split_once(':').ok_or_else(|| {
            format!("memory: bad frontmatter line `{line}` — want `key: value`")
        })?;
        let v = v.trim().trim_matches('"').trim();
        match k.trim() {
            "provenance" => meta.provenance = v.to_string(),
            "confidence" => {
                meta.confidence = v.parse::<f64>().map_err(|_| {
                    format!("memory: bad confidence `{v}` — want a number in 0..1")
                })?
            }
            "valid_from" => {
                meta.valid_from = if v.is_empty() { None } else { Some(v.to_string()) }
            }
            "valid_to" => {
                meta.valid_to = if v.is_empty() { None } else { Some(v.to_string()) }
            }
            "sensitivity" => meta.sensitivity = Sensitivity::parse(v)?,
            // Unknown keys are ignored (forward-compatible headers).
            _ => {}
        }
    }
    validate_meta(&meta)?;
    let mut body = all[end + 1..].join("\n");
    if text.ends_with('\n') {
        body.push('\n');
    }
    Ok((meta, body))
}

/// The one gate on entry metadata: confidence must be finite and in
/// 0..=1; validity bounds must be RFC3339 when present.
pub fn validate_meta(meta: &EntryMeta) -> Result<(), String> {
    if !meta.confidence.is_finite() || meta.confidence < 0.0 || meta.confidence > 1.0 {
        return Err(format!(
            "memory: confidence {} out of range — want 0..1",
            meta.confidence
        ));
    }
    for (name, v) in [
        ("valid_from", &meta.valid_from),
        ("valid_to", &meta.valid_to),
    ] {
        if let Some(s) = v {
            if !valid_rfc3339(s) {
                return Err(format!(
                    "memory: {name} `{s}` is not RFC3339 (want e.g. 2026-01-02T15:04:05Z)"
                ));
            }
        }
    }
    Ok(())
}

/// Structural RFC3339 check (`YYYY-MM-DDTHH:MM:SS[.frac](Z|±HH:MM)`,
/// ranges + day-of-month incl. leap years). Zero-dep by design — shape
/// validation, not a clock.
fn valid_rfc3339(s: &str) -> bool {
    let b = s.as_bytes();
    let mut i = 0usize;
    fn digits(b: &[u8], i: &mut usize, n: usize) -> Option<u32> {
        if b.len() < *i + n {
            return None;
        }
        let mut v = 0u32;
        for k in 0..n {
            let c = b[*i + k];
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + u32::from(c - b'0');
        }
        *i += n;
        Some(v)
    }
    fn lit(b: &[u8], i: &mut usize, c: u8) -> bool {
        if b.get(*i) == Some(&c) {
            *i += 1;
            true
        } else {
            false
        }
    }
    let year = digits(b, &mut i, 4);
    if !lit(b, &mut i, b'-') {
        return false;
    }
    let month = digits(b, &mut i, 2);
    if !lit(b, &mut i, b'-') {
        return false;
    }
    let day = digits(b, &mut i, 2);
    if !(lit(b, &mut i, b'T') || lit(b, &mut i, b't')) {
        return false;
    }
    let hour = digits(b, &mut i, 2);
    if !lit(b, &mut i, b':') {
        return false;
    }
    let min = digits(b, &mut i, 2);
    if !lit(b, &mut i, b':') {
        return false;
    }
    let sec = digits(b, &mut i, 2);
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    if b.get(i) == Some(&b'Z') || b.get(i) == Some(&b'z') {
        i += 1;
    } else if b.get(i) == Some(&b'+') || b.get(i) == Some(&b'-') {
        i += 1;
        let th = digits(b, &mut i, 2);
        if !lit(b, &mut i, b':') {
            return false;
        }
        let tm = digits(b, &mut i, 2);
        if th.map_or(true, |v| v > 23) || tm.map_or(true, |v| v > 59) {
            return false;
        }
    } else {
        return false;
    }
    if i != b.len() {
        return false;
    }
    let (y, mo, d, h, mi, s) = match (year, month, day, hour, min, sec) {
        (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) => (y, mo, d, h, mi, s),
        _ => return false,
    };
    if mo < 1 || mo > 12 || d < 1 || d > 31 {
        return false;
    }
    if h > 23 || mi > 59 || s > 60 {
        return false;
    }
    let dim = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        _ => return false,
    };
    d <= dim
}

/// Memory layer (P6-1 file convention): each layer is a subdirectory of
/// the memory dir holding that kind of topic file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Layer {
    Profile,
    Episodic,
    Semantic,
    Procedural,
}

impl Layer {
    pub const ALL: [Layer; 4] = [
        Layer::Profile,
        Layer::Episodic,
        Layer::Semantic,
        Layer::Procedural,
    ];

    /// Subdirectory name under the memory dir.
    pub const fn name(self) -> &'static str {
        match self {
            Layer::Profile => "profile",
            Layer::Episodic => "episodic",
            Layer::Semantic => "semantic",
            Layer::Procedural => "procedural",
        }
    }

    /// Minimum autonomy a session needs to write this layer: identity
    /// facts need approval; personal history and how-tos journal a
    /// receipt; world facts are low-risk.
    pub const fn write_bar(self) -> crate::perm::Autonomy {
        match self {
            Layer::Profile => crate::perm::Autonomy::ActWithApproval,
            Layer::Episodic => crate::perm::Autonomy::ActAndReport,
            Layer::Procedural => crate::perm::Autonomy::ActAndReport,
            Layer::Semantic => crate::perm::Autonomy::ActSilently,
        }
    }
}

/// Layer → autonomy write bar as data (mirrors `Layer::write_bar`).
pub const WRITE_BAR: [(Layer, crate::perm::Autonomy); 4] = [
    (Layer::Profile, crate::perm::Autonomy::ActWithApproval),
    (Layer::Episodic, crate::perm::Autonomy::ActAndReport),
    (Layer::Semantic, crate::perm::Autonomy::ActSilently),
    (Layer::Procedural, crate::perm::Autonomy::ActAndReport),
];

/// Prompt legend for the memory segment: layer dirs + header keys.
/// Static bytes (prefix-cache safe); the ≤200B cap is asserted in test.
pub const MEMORY_LEGEND: &str = "Layers: profile/ identity, episodic/ events, \
    semantic/ facts, procedural/ how-to. Headers: provenance, confidence 0-1, \
    sensitivity public|personal|secret.";

/// Create the memory dir, its 4 layer subdirs, and seed INDEX.md if
/// absent. Returns the index path.
pub fn ensure(dir: &Path) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    for layer in Layer::ALL {
        std::fs::create_dir_all(dir.join(layer.name()))?;
    }
    let idx = dir.join(INDEX_NAME);
    if !idx.exists() {
        std::fs::write(&idx, SEED_INDEX)?;
    }
    Ok(idx)
}

/// The system-prompt segment carrying the index — sits at the end of the
/// static region (Invariant 2): stable bytes when the index is unchanged,
/// and an edit only invalidates cache from this segment onward.
/// Re-read every turn because the model may have just edited it. An index
/// over the cap is truncated *with a repair note* — never silently.
pub fn index_segment(dir: &Path) -> String {
    let idx = dir.join(INDEX_NAME);
    let text = std::fs::read_to_string(&idx).unwrap_or_default();
    let (body, note) = if text.len() > INDEX_CAP {
        // Largest char-boundary byte offset still within the cap.
        let cut = text
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|end| *end <= INDEX_CAP)
            .last()
            .unwrap_or(0);
        (
            &text[..cut],
            "\n\n[overseer] INDEX.md exceeds 25KB — prune it: keep only \
             one-line pointers and move detail into topic files.",
        )
    } else {
        (text.as_str(), "")
    };
    format!(
        "## Memory index\n\
         `{}/` is your persistent memory — read and update it with ordinary \
         file tools. {INDEX_NAME} holds one-line pointers (≤25KB); details \
         live in topic files you create there.\n\n{body}{note}\n\n{MEMORY_LEGEND}",
        dir.display()
    )
}

/// Sensitivity-filtered index view (P6-2): the quarantined subagent
/// context shows only entries at or below `filter`. `Secret` topic
/// files stay in the index body only when the filter admits them;
/// `index_segment` is the unfiltered (Personal-default parent) path.
/// Filtering is line-scoped: a line names a layer file; its header
/// decides. Unresolvable lines pass through (fail-open for pointers,
/// fail-closed for bodies — the subagent has no write tools anyway).
pub fn index_segment_filtered(dir: &Path, filter: Sensitivity) -> String {
    let idx = dir.join(INDEX_NAME);
    let text = std::fs::read_to_string(&idx).unwrap_or_default();
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            let Some(name) = topic_name(line) else {
                return true;
            };
            match entry_sensitivity(dir, name) {
                // Missing/unreadable headers default to Personal.
                None => admits(filter, Sensitivity::Personal),
                Some(s) => admits(filter, s),
            }
        })
        .collect();
    let body = kept.join("\n");
    let (body, note) = if body.len() > INDEX_CAP {
        let cut = body
            .char_indices()
            .map(|(i, c)| i + c.len_utf8())
            .take_while(|end| *end <= INDEX_CAP)
            .last()
            .unwrap_or(0);
        (
            body[..cut].to_string(),
            "\n\n[overseer] INDEX.md exceeds 25KB — prune it: keep only \
             one-line pointers and move detail into topic files.",
        )
    } else {
        (body, "")
    };
    format!(
        "## Memory index\n\
         `{}/` is your persistent memory — read and update it with ordinary \
         file tools. {INDEX_NAME} holds one-line pointers (≤25KB); details \
         live in topic files you create there.\n\n{body}{note}\n\n{MEMORY_LEGEND}",
        dir.display()
    )
}

/// Ordering on sensitivity tiers: Public < Personal < Secret. A filter
/// admits every entry at or below its own tier.
pub fn admits(filter: Sensitivity, entry: Sensitivity) -> bool {
    rank(entry) <= rank(filter)
}

fn rank(s: Sensitivity) -> u8 {
    match s {
        Sensitivity::Public => 0,
        Sensitivity::Personal => 1,
        Sensitivity::Secret => 2,
    }
}

/// First `*.md` token on an index line, if any.
fn topic_name(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find(|tok| tok.ends_with(".md"))
        .map(|tok| tok.trim_matches(|c| c == '`' || c == '"' || c == '\'' || c == ',' || c == ';'))
        .filter(|tok| !tok.is_empty() && !tok.contains('/') && !tok.contains('\\'))
}

/// Sensitivity of a topic file from its frontmatter header. Searches
/// the layer subdirs as well as the memory root. None when the file
/// is missing or its header is unreadable (caller defaults).
fn entry_sensitivity(dir: &Path, name: &str) -> Option<Sensitivity> {
    let mut cands = vec![dir.join(name)];
    for layer in Layer::ALL {
        cands.push(dir.join(layer.name()).join(name));
    }
    for cand in cands {
        if let Ok(text) = std::fs::read_to_string(&cand) {
            if let Ok((meta, _)) = parse_meta(&text) {
                // A bare file (no frontmatter) parses to the default —
                // only trust an explicit header.
                if text.lines().next().map(|l| l.trim()) == Some("---") {
                    return Some(meta.sensitivity);
                }
                return None;
            }
            return None;
        }
    }
    None
}

/// Quarantine a memory entry without overwriting it (P6-2 ADD-only):
/// appends a `superseded_by <name>` trailer line to `path`. The old
/// content stays on disk and in git — consolidation never rewrites a
/// topic file smaller or deletes one.
pub fn invalidate(path: &Path, superseded_by: &str) -> std::io::Result<()> {
    let mut text = std::fs::read_to_string(path).unwrap_or_default();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(&format!("superseded_by {superseded_by}\n"));
    std::fs::write(path, text)
}

/// Locate a topic file by name: memory root first, then each layer
/// subdir. None when no backing file exists.
pub fn layer_path(dir: &Path, name: &str) -> Option<PathBuf> {
    let root = dir.join(name);
    if root.is_file() {
        return Some(root);
    }
    for layer in Layer::ALL {
        let p = dir.join(layer.name()).join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Git-version the memory dir. Runs `git init` once, then commits any dirty
/// state. Best-effort: memory works without history, so failures are
/// swallowed (no git binary, read-only fs) rather than killing the turn.
pub fn commit(dir: &Path, msg: &str) {
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    };
    if !dir.join(".git").exists() && !git(&["init", "-q"]) {
        return;
    }
    if !git(&["add", "-A"]) {
        return;
    }
    // diff --cached --quiet exits 1 when there's something to commit.
    if git(&["diff", "--cached", "--quiet"]) {
        return;
    }
    let _ = git(&[
        "-c",
        "user.name=overseer",
        "-c",
        "user.email=overseer@local",
        "commit",
        "-qm",
        msg,
    ]);
}

/// Files changed in the memory dir since the last engine commit (P6-2
/// audit signal): `git status --porcelain` paths, empty when clean or
/// when git is unavailable. Best-effort like `commit` — never errors.
pub fn dirty_files(dir: &Path) -> Vec<String> {
    if !dir.join(".git").exists() {
        return Vec::new();
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        // -uall: expand untracked dirs to file paths (`episodic/` →
        // `episodic/note.md`) so the audit event names real files.
        .args(["status", "--porcelain=v1", "-uall"])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            // Porcelain v1: `XY <path>[ -> <orig>]`.
            let path = l.get(3..)?.trim();
            let path = path.split(" -> ").last().unwrap_or(path).trim();
            let path = path.trim_matches('"');
            if path.is_empty() {
                None
            } else {
                Some(path.to_string())
            }
        })
        .collect()
}

/// Sleep-time consolidation (P3.8): a small-tier call that dedupes and
/// tightens `INDEX.md`, then a git commit. Topic files are read for
/// context but only the index is rewritten — merging topic bodies is the
/// model's job through ordinary edits, not a bulk engine rewrite.
///
/// The model returns the new index between `---INDEX---` markers; the
/// engine writes it (hard-capped) and reports what changed. Deterministic
/// fallback: a parse failure leaves the index untouched.
pub fn consolidate(
    provider: &dyn crate::provider::Provider,
    model: &str,
    dir: &Path,
) -> Result<String, String> {
    let idx = ensure(dir).map_err(|e| e.to_string())?;
    let old_index = std::fs::read_to_string(&idx).unwrap_or_default();

    // Topic files: bounded context for the dedupe pass.
    let mut topics = String::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "md") && e.file_name() != INDEX_NAME {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    let head: String = t.chars().take(2_000).collect();
                    topics.push_str(&format!(
                        "\n### {}\n{head}\n",
                        e.file_name().to_string_lossy()
                    ));
                }
            }
        }
    }

    let prompt = format!(
        "You are consolidating an agent's file-based memory. Below is \
         INDEX.md (one-line pointers) and the heads of the topic files.\n\
         Rewrite INDEX.md only: dedupe pointers, drop stale entries whose \
         topic file is gone, keep one line per topic in the form \
         `name.md — what it's about`. Validity: if a topic's content says \
         it expired or was superseded, drop its pointer.\n\
         ADD-only rules (no destructive rewrites): only append deltas or \
         drop stale pointers — never rewrite a topic file smaller and \
         never overwrite a quarantined entry; mark superseded entries \
         with a `superseded_by` trailer instead of deleting them; keep \
         entries whose validity window still covers now.\n\
         Reply with the full new index between ---INDEX--- markers.\n\n\
         == CURRENT INDEX.md ==\n{old_index}\n== TOPIC HEADS =={topics}"
    );
    let msgs = [crate::ir::Message::user_text(prompt)];
    let req = crate::provider::Request {
        model,
        system: &[],
        tools: &[],
        messages: &msgs,
        max_tokens: 4_096,
        thinking_budget: None,
        effort: Some(crate::provider::Effort::Min),
        cache_breakpoints: false,
    };
    let resp = provider
        .complete(&req)
        .map_err(|e| format!("consolidate: {e}"))?;
    let text: String = resp
        .blocks
        .iter()
        .filter_map(|b| match b {
            crate::ir::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    let new_index = text
        .split("---INDEX---")
        .nth(1)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "consolidate: model reply had no ---INDEX--- section".to_string())?;
    let capped: String = new_index.chars().take(INDEX_CAP).collect();
    std::fs::write(&idx, format!("{capped}\n")).map_err(|e| e.to_string())?;
    commit(dir, "consolidate");

    let dropped = old_index
        .lines()
        .filter(|l| l.contains(".md"))
        .filter(|l| !new_index.contains(l.trim()))
        .count();
    Ok(format!(
        "consolidated: {} → {} index lines, {dropped} pointers dropped",
        old_index.lines().count(),
        capped.lines().count()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-mem-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn ensure_seeds_index() {
        let dir = tmpdir().join("memory");
        let idx = ensure(&dir).unwrap();
        assert!(idx.exists());
        let text = std::fs::read_to_string(&idx).unwrap();
        assert!(text.contains("Memory Index"));
    }

    #[test]
    fn segment_carries_index_and_cap_note() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "facts.md — user facts\n").unwrap();
        let seg = index_segment(&dir);
        assert!(seg.contains("## Memory index"));
        assert!(seg.contains("facts.md — user facts"));
        assert!(!seg.contains("exceeds 25KB"));

        // Over-cap index → truncated + repair note.
        std::fs::write(&idx, "x".repeat(INDEX_CAP + 100)).unwrap();
        let seg = index_segment(&dir);
        assert!(seg.contains("exceeds 25KB"));
        assert!(seg.len() < INDEX_CAP + 1_000);
    }

    #[test]
    fn commit_versions_changes() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        commit(&dir, "seed");
        assert!(dir.join(".git").exists());
        std::fs::write(dir.join("facts.md"), "likes rust\n").unwrap();
        commit(&dir, "add facts");
        // Log should have two commits.
        let out = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["log", "--oneline"])
            .output()
            .unwrap();
        let log = String::from_utf8_lossy(&out.stdout);
        assert_eq!(log.lines().count(), 2);
    }

    struct FixedProvider {
        reply: String,
    }
    impl crate::provider::Provider for FixedProvider {
        fn complete(
            &self,
            req: &crate::provider::Request,
        ) -> Result<crate::provider::Response, crate::provider::ProviderError> {
            assert_eq!(req.effort, Some(crate::provider::Effort::Min));
            Ok(crate::provider::Response {
                blocks: vec![crate::ir::Block::Text {
                    text: self.reply.clone(),
                }],
                stop_reason: crate::provider::StopReason::EndTurn,
                usage: crate::ir::Usage::default(),
                request_bytes: 0,
                latency_ms: 0,
            })
        }
        fn name(&self) -> &'static str {
            "fixed"
        }
    }

    #[test]
    fn consolidate_rewrites_index_and_commits() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "# Memory Index\n\nfacts.md — old\ndupe.md — old\n").unwrap();
        std::fs::write(dir.join("facts.md"), "data").unwrap();
        let p = FixedProvider {
            reply: "---INDEX---\n# Memory Index\n\nfacts.md — user facts\n---INDEX---".into(),
        };
        let msg = consolidate(&p, "tiny", &dir).unwrap();
        assert!(msg.contains("consolidated"));
        let new = std::fs::read_to_string(&idx).unwrap();
        assert!(new.contains("facts.md — user facts"));
        assert!(!new.contains("dupe.md"), "stale pointer dropped");
        assert!(dir.join(".git").exists(), "consolidate commits");
    }

    #[test]
    fn consolidate_parse_failure_leaves_index() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "original\n").unwrap();
        let p = FixedProvider {
            reply: "no markers here".into(),
        };
        assert!(consolidate(&p, "tiny", &dir).is_err());
        assert_eq!(std::fs::read_to_string(&idx).unwrap(), "original\n");
    }

    #[test]
    fn headers_valid_and_invalid() {
        let good = "---\nprovenance: interview answer-3\nconfidence: 0.8\n\
            valid_from: 2026-01-02T15:04:05Z\nsensitivity: secret\n---\nlikes rust\n";
        let (meta, body) = parse_meta(good).unwrap();
        assert_eq!(meta.provenance, "interview answer-3");
        assert_eq!(meta.confidence, 0.8);
        assert_eq!(meta.valid_from.as_deref(), Some("2026-01-02T15:04:05Z"));
        assert_eq!(meta.sensitivity, Sensitivity::Secret);
        assert_eq!(body, "likes rust\n");

        // No frontmatter → default meta, whole text is body.
        let (meta, body) = parse_meta("plain body\n").unwrap();
        assert_eq!(meta, EntryMeta::default());
        assert_eq!(body, "plain body\n");

        // Unterminated frontmatter names the fault.
        assert!(parse_meta("---\nprovenance: x\n").is_err());
        // Bad sensitivity spelling.
        assert!(parse_meta("---\nsensitivity: topsecret\n---\nbody\n").is_err());
        // Bad frontmatter line shape.
        assert!(parse_meta("---\nnot a kv line\n---\nbody\n").is_err());
    }

    #[test]
    fn validator_rejects_confidence_and_dates() {
        // Confidence above 1.
        assert!(
            parse_meta("---\nconfidence: 1.5\n---\nbody\n").is_err(),
            "confidence>1 must fail"
        );
        // Negative, NaN, non-numeric.
        assert!(parse_meta("---\nconfidence: -0.1\n---\nbody\n").is_err());
        assert!(parse_meta("---\nconfidence: nan\n---\nbody\n").is_err());
        assert!(parse_meta("---\nconfidence: lots\n---\nbody\n").is_err());
        assert!(
            validate_meta(&EntryMeta {
                confidence: f64::INFINITY,
                ..EntryMeta::default()
            })
            .is_err()
        );
        // Boundary values pass.
        assert!(validate_meta(&EntryMeta {
            confidence: 0.0,
            ..EntryMeta::default()
        })
        .is_ok());
        assert!(validate_meta(&EntryMeta {
            confidence: 1.0,
            ..EntryMeta::default()
        })
        .is_ok());
        // Bad dates: wrong shape, month 13, Feb 30, missing zone.
        for bad in [
            "not-a-date",
            "2026-13-01T00:00:00Z",
            "2026-02-30T00:00:00Z",
            "2026-01-02 15:04:05",
            "2026-01-02T25:00:00Z",
        ] {
            let text = format!("---\nvalid_from: {bad}\n---\nbody\n");
            assert!(parse_meta(&text).is_err(), "date `{bad}` must fail");
        }
        // Good dates: Z, offset, fractional, leap day.
        for good in [
            "2026-01-02T15:04:05Z",
            "2026-01-02T15:04:05+02:00",
            "2026-01-02T15:04:05.123Z",
            "2024-02-29T00:00:00Z",
        ] {
            let text = format!("---\nvalid_from: {good}\n---\nbody\n");
            assert!(parse_meta(&text).is_ok(), "date `{good}` must pass");
        }
    }

    #[test]
    fn ensure_creates_layer_dirs() {
        let dir = tmpdir().join("memory");
        ensure(&dir).unwrap();
        for layer in Layer::ALL {
            assert!(
                dir.join(layer.name()).is_dir(),
                "layer dir {} missing",
                layer.name()
            );
        }
        assert!(dir.join(INDEX_NAME).exists());
    }

    #[test]
    fn write_bar_map_covers_all_layers() {
        use crate::perm::Autonomy;
        assert_eq!(WRITE_BAR.len(), 4);
        assert_eq!(
            WRITE_BAR
                .iter()
                .map(|(l, _)| *l)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            4,
            "one bar entry per layer"
        );
        // Table and method agree.
        for (layer, bar) in WRITE_BAR {
            assert_eq!(layer.write_bar(), bar);
        }
        // Identity facts need approval; world facts are low-risk.
        assert_eq!(Layer::Profile.write_bar(), Autonomy::ActWithApproval);
        assert_eq!(Layer::Semantic.write_bar(), Autonomy::ActSilently);
    }

    #[test]
    fn legend_block_stays_small() {
        // R1 token-vagueness fix: the legend is a hard byte cap, not a
        // token estimate.
        assert!(
            MEMORY_LEGEND.len() <= 200,
            "legend is {} bytes, cap is 200",
            MEMORY_LEGEND.len()
        );
        assert!(MEMORY_LEGEND.contains("profile/"));
        assert!(MEMORY_LEGEND.contains("sensitivity"));
        // The segment carries it.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        assert!(index_segment(&dir).contains(MEMORY_LEGEND));
    }

    #[test]
    fn filtered_view_hides_secret() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        std::fs::write(
            dir.join("episodic").join("diary.md"),
            "---\nsensitivity: personal\n---\nhad lunch\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantic").join("token.md"),
            "---\nsensitivity: secret\n---\nsk-abc\n",
        )
        .unwrap();
        std::fs::write(
            &dir.join(INDEX_NAME),
            "# Memory Index\n\ndiary.md — lunch notes\ntoken.md — api token\n",
        )
        .unwrap();
        let personal = index_segment_filtered(&dir, Sensitivity::Personal);
        assert!(personal.contains("diary.md"), "{personal}");
        assert!(!personal.contains("token.md"), "{personal}");
        let secret = index_segment_filtered(&dir, Sensitivity::Secret);
        assert!(secret.contains("diary.md"));
        assert!(secret.contains("token.md"));
        let public = index_segment_filtered(&dir, Sensitivity::Public);
        assert!(!public.contains("diary.md"), "{public}");
        assert!(!public.contains("token.md"), "{public}");
    }

    #[test]
    fn invalidate_appends_trailer() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        let p = dir.join("episodic").join("old.md");
        std::fs::write(&p, "old content\n").unwrap();
        invalidate(&p, "new.md").unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert!(text.contains("old content"), "history preserved: {text}");
        assert!(text.contains("superseded_by new.md"), "{text}");
    }

    #[test]
    fn consolidate_prompt_carries_add_only_rules() {
        // ADD-only is a prompt contract: the consolidation instruction
        // must forbid rewrite-smaller/quarantine-overwrite. The live
        // behavior half is covered by the audit accept tests (dirty→
        // event, clean→none) in the agent suite.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        // Capture the prompt the engine sends.
        struct Spy {
            seen: std::sync::Mutex<Vec<String>>,
        }
        impl crate::provider::Provider for Spy {
            fn complete(
                &self,
                req: &crate::provider::Request,
            ) -> Result<crate::provider::Response, crate::provider::ProviderError> {
                let t: String = req.messages.iter().map(|m| m.text()).collect();
                self.seen.lock().unwrap().push(t);
                Ok(crate::provider::Response {
                    blocks: vec![crate::ir::Block::Text {
                        text: "---INDEX---\n# Memory Index\n\nfacts.md — kept\n---INDEX---"
                            .into(),
                    }],
                    stop_reason: crate::provider::StopReason::EndTurn,
                    usage: crate::ir::Usage::default(),
                    request_bytes: 0,
                    latency_ms: 0,
                })
            }
            fn name(&self) -> &'static str {
                "spy"
            }
        }
        let spy = Spy {
            seen: std::sync::Mutex::new(Vec::new()),
        };
        consolidate(&spy, "tiny", &dir).unwrap();
        let seen = spy.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert!(seen[0].contains("ADD-only"), "{}", seen[0]);
        assert!(seen[0].contains("superseded_by"), "{}", seen[0]);
        assert!(seen[0].contains("quarantine"), "{}", seen[0]);
    }
}
