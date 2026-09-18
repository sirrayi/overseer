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
/// Ceiling on a declared per-asset TTL (100 years — a bound, not a policy).
pub const MAX_TTL_DAYS: u64 = 36_500;
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

/// Retention class of one memory asset (TencentDB TTL-taxonomy pattern,
/// arsenal B2): `private` is the default, `shared` may be read by anything
/// the session is authorized to share with, and `regulated` is never
/// served into a subagent/quarantined view regardless of its sensitivity
/// tier — the column exists so an operator can mark an asset "keep on
/// disk, never fan out" without deleting it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Governance {
    #[default]
    Private,
    Shared,
    Regulated,
}

impl Governance {
    pub const fn as_str(self) -> &'static str {
        match self {
            Governance::Private => "private",
            Governance::Shared => "shared",
            Governance::Regulated => "regulated",
        }
    }

    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_lowercase().as_str() {
            "private" => Ok(Governance::Private),
            "shared" => Ok(Governance::Shared),
            "regulated" => Ok(Governance::Regulated),
            other => Err(format!(
                "memory: bad governance `{other}` — want private|shared|regulated"
            )),
        }
    }
}

/// File-convention header for one memory topic file (P6-1): frontmatter
/// between `---` lines carrying provenance, a 0..1 confidence, an
/// optional RFC3339 validity window, a sensitivity tier, and the B2 TTL
/// taxonomy columns (`ttl_days` retention, `governance` class). Files
/// without frontmatter read as the unvetted default.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryMeta {
    pub provenance: String,
    pub confidence: f64,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub sensitivity: Sensitivity,
    /// Per-asset time-to-live in days, counted from the file's mtime (the
    /// engine has no creation timestamp and refuses to invent one). `None`
    /// = keep until superseded. `Some(0)` = expires immediately, which is
    /// how a short-lived note is marked.
    pub ttl_days: Option<u64>,
    /// Retention class (see `Governance`).
    pub governance: Governance,
}

impl Default for EntryMeta {
    fn default() -> Self {
        EntryMeta {
            provenance: String::new(),
            confidence: 0.5,
            valid_from: None,
            valid_to: None,
            sensitivity: Sensitivity::Personal,
            ttl_days: None,
            governance: Governance::Private,
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
        let (k, v) = line
            .split_once(':')
            .ok_or_else(|| format!("memory: bad frontmatter line `{line}` — want `key: value`"))?;
        let v = v.trim().trim_matches('"').trim();
        match k.trim() {
            "provenance" => meta.provenance = v.to_string(),
            "confidence" => {
                meta.confidence = v
                    .parse::<f64>()
                    .map_err(|_| format!("memory: bad confidence `{v}` — want a number in 0..1"))?
            }
            "valid_from" => {
                meta.valid_from = if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            "valid_to" => {
                meta.valid_to = if v.is_empty() {
                    None
                } else {
                    Some(v.to_string())
                }
            }
            "sensitivity" => meta.sensitivity = Sensitivity::parse(v)?,
            "ttl_days" => {
                meta.ttl_days = Some(v.parse::<u64>().map_err(|_| {
                    format!("memory: bad ttl_days `{v}` — want a whole number of days")
                })?)
            }
            "governance" => meta.governance = Governance::parse(v)?,
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
    if let Some(d) = meta.ttl_days {
        if d > MAX_TTL_DAYS {
            return Err(format!(
                "memory: ttl_days {d} out of range — want 0..={MAX_TTL_DAYS}"
            ));
        }
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
        if th.is_none_or(|v| v > 23) || tm.is_none_or(|v| v > 59) {
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
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
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

/// Map a write/edit target to its memory layer's write bar (F5).
/// None when the target is outside `memory_dir` (or no memory dir): the
/// lane default decides. Pure path-prefix check — no fs access.
pub fn layer_bar_for_path(
    memory_dir: Option<&std::path::Path>,
    root: &std::path::Path,
    target: &str,
) -> Option<crate::perm::Autonomy> {
    let mem = memory_dir?;
    let resolved = if std::path::Path::new(target).is_absolute() {
        std::path::PathBuf::from(target)
    } else {
        root.join(target)
    };
    let rel = resolved.strip_prefix(mem).ok()?;
    let first = rel
        .components()
        .next()?
        .as_os_str()
        .to_string_lossy()
        .to_string();
    let layer = Layer::ALL.iter().find(|l| l.name() == first)?;
    Some(layer.write_bar())
}

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
/// True when an INDEX pointer line names a quarantine proposal: unreviewed
/// untrusted text (RT-3). Proposals stay on disk for human review but are
/// never injected into the trusted memory segment.
/// Lexicographic RFC-3339 expiry check (UTC `now`): valid_to past → expired.
/// No chrono dep — RFC-3339 UTC strings compare lexicographically.
fn meta_expired(meta: &EntryMeta) -> bool {
    let Some(to) = &meta.valid_to else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Format now as RFC-3339-ish UTC for string comparison via the same
    // lexicographic property: compare against the stored string's prefix.
    // Simplest sound rule: a valid_to strictly earlier than the current
    // year-month-day prefix chain — compare full strings against a
    // now-formatted stamp built without chrono.
    let stamp = format_utc_stamp(now);
    to.as_str() < stamp.as_str()
}

fn format_utc_stamp(secs: u64) -> String {
    // Days since epoch → civil date (Howard Hinnant's algorithm), UTC.
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    let sod = secs % 86_400;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        y,
        m,
        d,
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// True when the topic body carries a `superseded_by` trailer pointing at
/// a live successor (F9 — `invalidate()` appends these; the pointer is
/// stale). Both spellings are accepted: `superseded_by: name` (the
/// frontmatter-ish form) and `superseded_by name` (exactly what
/// `invalidate()` writes). Requiring only the colon was the F9 gap this
/// port closes: an invalidated asset stayed "live" to every reader.
fn meta_superseded(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim();
        if let Some(rest) = t.strip_prefix("superseded_by:") {
            return !rest.trim().is_empty();
        }
        if let Some(rest) = t.strip_prefix("superseded_by ") {
            return !rest.trim().is_empty();
        }
        false
    })
}

/// True when an asset's declared TTL has run out: `mtime + ttl_days` is
/// at or before now. No creation timestamp is stored (the engine refuses
/// to invent one), so the file's mtime is the retention clock — editing an
/// asset renews it, which is the behavior an operator expects from a
/// "keep this for N days" column. `mtime: None` (stat failed) → not
/// expired: fail-open on liveness, fail-closed on bodies.
fn meta_ttl_expired(meta: &EntryMeta, mtime: Option<std::time::SystemTime>) -> bool {
    let Some(days) = meta.ttl_days else {
        return false;
    };
    let Some(mtime) = mtime else {
        return false;
    };
    let Ok(modified) = mtime.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    modified
        .as_secs()
        .saturating_add(days.saturating_mul(86_400))
        <= now
}

/// True when an entry's header says it is still current (validity window
/// and TTL not elapsed, not superseded).
fn meta_current(meta: &EntryMeta, text: &str, mtime: Option<std::time::SystemTime>) -> bool {
    if meta_expired(meta) {
        return false;
    }
    if meta_ttl_expired(meta, mtime) {
        return false;
    }
    !meta_superseded(text)
}

/// One topic file's parse state — the single scanner every liveness,
/// sensitivity, governance, and view decision reads.
enum Topic {
    /// No backing file (orphan pointer).
    Missing,
    /// File with no frontmatter: documented defaults apply (Personal /
    /// Private) and the whole text is the body.
    Bare(String),
    /// File with a valid header.
    Headed(EntryMeta, String),
    /// File whose header does not parse: fail closed (Secret /
    /// Regulated) — an unreadable header must not open anything up. The
    /// body is deliberately not carried: nothing may serve it.
    Malformed,
}

fn topic_of(dir: &Path, name: &str) -> (Topic, Option<std::time::SystemTime>) {
    let Some(path) = layer_path(dir, name) else {
        return (Topic::Missing, None);
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return (Topic::Missing, None);
    };
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    if text.lines().next().map(|l| l.trim()) != Some("---") {
        return (Topic::Bare(text), mtime);
    }
    match parse_meta(&text) {
        Ok((meta, _)) => (Topic::Headed(meta, text), mtime),
        Err(_) => (Topic::Malformed, mtime),
    }
}

/// True when a topic body is still current: not expired (`valid_to` or the
/// B2 `ttl_days` clock) and not superseded. This is the entry-level half
/// of the zep validity-interval filter — the INDEX half (`pointer_live`)
/// only drops pointers; bodies are what a fetch actually returns.
pub fn entry_valid(text: &str, mtime: Option<std::time::SystemTime>) -> bool {
    match parse_meta(text) {
        Ok((meta, _)) => meta_current(&meta, text, mtime),
        // Unparsable header: fail closed (not served as current).
        Err(_) => false,
    }
}

/// Read a topic file by name, refusing expired/superseded content.
/// `None` when the file is missing or no longer current — the caller then
/// behaves as if the pointer were stale.
pub fn topic_text(dir: &Path, name: &str) -> Option<String> {
    let (topic, mtime) = topic_of(dir, name);
    match topic {
        Topic::Bare(text) => entry_valid(&text, mtime).then_some(text),
        Topic::Headed(meta, text) => meta_current(&meta, &text, mtime).then_some(text),
        Topic::Missing | Topic::Malformed => None,
    }
}

/// What a subagent view should do with one INDEX pointer line (P8-B).
pub enum View {
    /// Serve the pointer and copy this body into the filtered dir.
    Admitted(String),
    /// Keep the pointer line, copy nothing (orphan pointer — stale-pointer
    /// hygiene belongs to `consolidate`, not the view).
    PointerOnly,
    /// Drop the pointer entirely: expired, superseded, above the
    /// sensitivity ceiling, or `regulated`.
    Hidden,
}

/// The one rule for what enters a quarantined (subagent) memory view: the
/// asset must be current (`entry_valid`), its sensitivity at or below
/// `filter`, and its governance class anything but `Regulated`. Written
/// once and used by both the index view and the copied-body view so the
/// two can never disagree.
pub fn subagent_view(dir: &Path, name: &str, filter: Sensitivity) -> View {
    let (topic, mtime) = topic_of(dir, name);
    match topic {
        // Missing file: orphan pointer — keep the line, copy nothing.
        Topic::Missing => View::PointerOnly,
        Topic::Bare(text) => {
            if entry_valid(&text, mtime) {
                View::Admitted(text)
            } else {
                View::Hidden
            }
        }
        Topic::Headed(meta, text) => {
            if !meta_current(&meta, &text, mtime) {
                return View::Hidden;
            }
            if meta.governance == Governance::Regulated || !admits(filter, meta.sensitivity) {
                View::Hidden
            } else {
                View::Admitted(text)
            }
        }
        // Unparsable header: fail closed (Malformed never admits).
        Topic::Malformed => View::Hidden,
    }
}

/// True when an INDEX pointer still names live content: the topic file's
/// own header must not say expired (valid_to past or TTL elapsed) or
/// superseded (superseded_by trailer). Unresolvable lines pass through
/// (F8 — the caller decides; bodies stay fail-closed elsewhere).
fn pointer_live(dir: &Path, line: &str) -> bool {
    let Some(name) = topic_name(line) else {
        return true;
    };
    // Layer-aware lookup (F9): topics live in profile/episodic/semantic/
    // procedural — a root-only join misses every layered file (always-live).
    let Some(path) = layer_path(dir, name) else {
        return true;
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return true;
    };
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    // Unparsable headers fail closed elsewhere; for liveness, only an
    // affirmative expired/superseded signal drops the pointer.
    if let Ok((meta, _)) = parse_meta(&text) {
        if !meta_current(&meta, &text, mtime) {
            return false;
        }
    }
    true
}

fn is_proposal_pointer(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("proposals/") || t.contains("proposals/")
}

pub fn index_segment(dir: &Path) -> String {
    let idx = dir.join(INDEX_NAME);
    let text = std::fs::read_to_string(&idx).unwrap_or_default();
    // RT-3: drop proposal pointers (unreviewed) + expired/superseded (F9).
    let text: String = text
        .lines()
        .filter(|l| !is_proposal_pointer(l) && pointer_live(dir, l))
        .collect::<Vec<_>>()
        .join("\n");
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
            // RT-3: proposals never enter any view.
            if is_proposal_pointer(line) {
                return false;
            }
            let Some(name) = topic_name(line) else {
                // F8: orphan/unresolvable pointer lines pass through
                // (fail-open for pointers, fail-closed for bodies).
                return true;
            };
            // P8-B: one rule for the index view and the copied bodies —
            // current, within the sensitivity ceiling, not regulated.
            match subagent_view(dir, name, filter) {
                View::Admitted(_) | View::PointerOnly => true,
                View::Hidden => false,
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

    // Topic files: bounded context for the dedupe pass. Walks the top
    // level AND the P6-1 layer subdirs (F6: after layering, topics live in
    // profile/episodic/semantic/procedural — a top-level-only scan judges
    // every layer pointer blind).
    let mut topics = String::new();
    let mut topic_dirs = vec![dir.to_path_buf()];
    for layer in Layer::ALL {
        topic_dirs.push(dir.join(layer.name()));
    }
    for tdir in topic_dirs {
        let Ok(entries) = std::fs::read_dir(&tdir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "md") && e.file_name() != INDEX_NAME {
                if let Ok(t) = std::fs::read_to_string(&p) {
                    let head: String = t.chars().take(2_000).collect();
                    let rel = p
                        .strip_prefix(dir)
                        .map(|r| r.display().to_string())
                        .unwrap_or_else(|_| e.file_name().to_string_lossy().to_string());
                    topics.push_str(&format!("\n### {rel}\n{head}\n"));
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
        seen: std::sync::Mutex<Vec<String>>,
    }
    impl FixedProvider {
        fn with_reply(reply: &str) -> Self {
            Self {
                reply: reply.into(),
                seen: std::sync::Mutex::new(Vec::new()),
            }
        }
    }
    impl crate::provider::Provider for FixedProvider {
        fn complete(
            &self,
            req: &crate::provider::Request,
        ) -> Result<crate::provider::Response, crate::provider::ProviderError> {
            assert_eq!(req.effort, Some(crate::provider::Effort::Min));
            let prompt: String = req
                .messages
                .iter()
                .flat_map(|m| {
                    m.content.iter().filter_map(|b| match b {
                        crate::ir::Block::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                })
                .collect();
            self.seen.lock().unwrap().push(prompt);
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
    fn consolidate_sees_layer_dir_topics() {
        // F6: topics in profile/episodic/semantic/procedural reach the prompt.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        std::fs::write(
            dir.join("semantic/facts.md"),
            "EXPIRED-MARKER-CONTENT sky-blue",
        )
        .unwrap();
        let p = FixedProvider::with_reply("---INDEX---\n# Memory Index\n---INDEX---");
        let _ = consolidate(&p, "tiny", &dir).unwrap();
        let seen = p.seen.lock().unwrap().join("\n");
        assert!(
            seen.contains("EXPIRED-MARKER-CONTENT"),
            "layer topic must reach the prompt, got: {}",
            &seen[..seen.len().min(500)]
        );
        assert!(seen.contains("semantic/facts.md"), "layer-relative name");
    }

    #[test]
    fn consolidate_rewrites_index_and_commits() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "# Memory Index\n\nfacts.md — old\ndupe.md — old\n").unwrap();
        std::fs::write(dir.join("facts.md"), "data").unwrap();
        let p = FixedProvider::with_reply(
            "---INDEX---\n# Memory Index\n\nfacts.md — user facts\n---INDEX---",
        );
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
        let p = FixedProvider::with_reply("no markers here");
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
        assert!(validate_meta(&EntryMeta {
            confidence: f64::INFINITY,
            ..EntryMeta::default()
        })
        .is_err());
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
            dir.join(INDEX_NAME),
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
    fn ttl_taxonomy_columns_parse_and_expire() {
        // P8-B accept (TencentDB TTL taxonomy): per-asset retention and
        // governance are header columns with hard validation.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        let (meta, _) = parse_meta(
            "---\nprovenance: note\nconfidence: 0.9\nttl_days: 30\ngovernance: shared\n---\nbody\n",
        )
        .unwrap();
        assert_eq!(meta.ttl_days, Some(30));
        assert_eq!(meta.governance, Governance::Shared);
        // Defaults when absent.
        assert_eq!(EntryMeta::default().ttl_days, None);
        assert_eq!(EntryMeta::default().governance, Governance::Private);
        // Bad values name the fault; the TTL has a hard ceiling.
        assert!(parse_meta("---\nttl_days: soon\n---\nbody\n")
            .unwrap_err()
            .contains("ttl_days"));
        assert!(parse_meta("---\ngovernance: secretish\n---\nbody\n")
            .unwrap_err()
            .contains("governance"));
        assert!(
            parse_meta(&format!("---\nttl_days: {}\n---\nbody\n", MAX_TTL_DAYS + 1)).is_err(),
            "ttl beyond the ceiling is rejected"
        );

        // TTL liveness runs off the file's mtime: a 30-day asset written
        // now is current, a 0-day asset is expired at once.
        std::fs::write(
            dir.join("semantic").join("live.md"),
            "---\nttl_days: 30\n---\nstill good\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantic").join("gone.md"),
            "---\nttl_days: 0\n---\nshort lived\n",
        )
        .unwrap();
        assert!(topic_text(&dir, "live.md").is_some());
        assert!(
            topic_text(&dir, "gone.md").is_none(),
            "ttl_days=0 has expired"
        );
        assert!(topic_text(&dir, "missing.md").is_none());

        // The INDEX drops the expired pointer but keeps the live one.
        std::fs::write(
            dir.join(INDEX_NAME),
            "# Memory Index\n\nlive.md — good\ngone.md — expired\n",
        )
        .unwrap();
        let seg = index_segment(&dir);
        assert!(seg.contains("live.md"), "{seg}");
        assert!(!seg.contains("gone.md"), "expired asset dropped: {seg}");
    }

    #[test]
    fn subagent_view_hides_expired_superseded_and_regulated_assets() {
        // P8-B accept (zep validity + TTL governance): the entry-level half
        // of the filter — bodies, not just pointers — and the regulated
        // column, which outranks the sensitivity ceiling.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        std::fs::write(dir.join("semantic").join("ok.md"), "plain body\n").unwrap();
        std::fs::write(
            dir.join("semantic").join("expired.md"),
            "---\nvalid_to: 2000-01-01T00:00:00Z\n---\nold\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantic").join("dead.md"),
            "---\nttl_days: 0\n---\nshort\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantic").join("superseded.md"),
            "---\nprovenance: x\n---\nold\nsuperseded_by ok.md\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantic").join("reg.md"),
            "---\ngovernance: regulated\n---\ncompliance material\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("semantic").join("secret.md"),
            "---\nsensitivity: secret\n---\nhush\n",
        )
        .unwrap();

        let personal = Sensitivity::Personal;
        assert!(matches!(
            subagent_view(&dir, "ok.md", personal),
            View::Admitted(_)
        ));
        for hidden in [
            "expired.md",
            "dead.md",
            "superseded.md",
            "reg.md",
            "secret.md",
        ] {
            assert!(
                matches!(subagent_view(&dir, hidden, personal), View::Hidden),
                "{hidden} must not enter the quarantined view"
            );
        }
        // Orphan pointer: keep the line, copy nothing.
        assert!(matches!(
            subagent_view(&dir, "orphan.md", personal),
            View::PointerOnly
        ));
        // A Secret ceiling admits the secret (Regulated still hidden).
        assert!(matches!(
            subagent_view(&dir, "secret.md", Sensitivity::Secret),
            View::Admitted(_)
        ));
        assert!(matches!(
            subagent_view(&dir, "reg.md", Sensitivity::Secret),
            View::Hidden
        ));

        // The index view agrees with the body view — one rule, two views.
        std::fs::write(
            dir.join(INDEX_NAME),
            "# Memory Index\n\nok.md — fine\nreg.md — compliance\ndead.md — short\n",
        )
        .unwrap();
        let seg = index_segment_filtered(&dir, personal);
        assert!(seg.contains("ok.md"), "{seg}");
        assert!(!seg.contains("reg.md"), "{seg}");
        assert!(!seg.contains("dead.md"), "{seg}");
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
        // The trailer `invalidate` writes must actually retire the asset —
        // the reader accepts the space form, so the pointer drops and the
        // body stops being served (F9: requiring a colon left it "live").
        assert!(
            topic_text(&dir, "old.md").is_none(),
            "superseded body served"
        );
        std::fs::write(dir.join(INDEX_NAME), "# Memory Index\n\nold.md — stale\n").unwrap();
        assert!(
            !index_segment(&dir).contains("old.md"),
            "superseded pointer survived the index view"
        );
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
                        text: "---INDEX---\n# Memory Index\n\nfacts.md — kept\n---INDEX---".into(),
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
