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
//!
//! Two P8-C ports (memory-graph wave), both pure over the same file
//! convention and both reusing this module's liveness rules — a port never
//! invents a second way to read the dir:
//!
//! - **mcp-mem-server `search_memory`** — deterministic lexical search over
//!   the live topic files: a scored, capped, name-tie-broken result set
//!   carrying the quotable line so a hit can be re-read instead of trusted.
//! - **LanceDB frontmatter prefilter** — the `WHERE` half of a vector
//!   search: header columns decide who is rankable, *before* scoring, so a
//!   filtered-out asset can never occupy a result slot.
// DEFERRED(owner): vector embeddings, an ANN/LanceDB index, and body-level
// ranking — this port lands the prefilter plus lexical scoring only; the
// delivery gate forbids new dependencies and a real index needs one.

use std::path::{Path, PathBuf};
use std::process::Command;

pub const INDEX_NAME: &str = "INDEX.md";
/// Resident core file (Letta's `core_memory` pattern, arsenal B2): a small
/// `CORE.md` always in the prompt, next to the index. Where INDEX.md is a
/// pointer table that grows, CORE.md is the fixed handful of lines that
/// must never be paged out — identity and standing instructions.
pub const CORE_NAME: &str = "CORE.md";
/// Hard cap on the resident core block (2KB ≈ 500 tokens). Over the cap the
/// block is truncated *with a repair note*, never silently.
pub const CORE_CAP: usize = 2_048;
/// Routing hint (LightRAG pattern, arsenal B2): level-aware retrieval is a
/// prompt contract, not code — tell the model which layer answers which
/// kind of question, and the local/global split falls out of the files.
pub const ROUTING_HINT: &str = "Retrieval: answer specific questions from \
    topic files (`read` them); answer overview questions from this index.";

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
         live in topic files you create there.\n\n{ROUTING_HINT}\n\n{body}{note}\n\n{MEMORY_LEGEND}{core}",
        dir.display(),
        core = core_block(dir)
    )
}

/// The resident core block (Letta `CORE.md`): the always-in-context handful
/// of lines, capped with a repair note when over budget. Empty when the
/// file is absent — the block is opt-in by existence, like everything else
/// in the memory dir. Parent view only: `index_segment_filtered` (the
/// quarantined subagent view) never carries it — core memory is not
/// scoped per entry, so there is no sensitivity ceiling to apply to it.
fn core_block(dir: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(dir.join(CORE_NAME)) else {
        return String::new();
    };
    let text = text.trim();
    if text.is_empty() {
        return String::new();
    }
    // Capped through the shared `core_budget` helper (letta P1): the
    // rendered bytes are unchanged — truncation plus repair note.
    let (kept, note) = core_budget(text, CORE_CAP);
    match note {
        None => format!("\n\n## Memory core ({CORE_NAME})\n{kept}"),
        Some(note) => format!("\n\n## Memory core ({CORE_NAME})\n{kept}\n\n{note}"),
    }
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

/// A reconciliation plan over the memory dir (mem0 pattern, arsenal B2):
/// what the index claims versus what the directory actually holds, decided
/// before any model call. Three verdicts, no model involved:
///
/// - `drop` — a pointer whose asset is missing, expired, or superseded;
/// - `add`  — a topic file that no pointer names;
/// - `keep` — a pointer and its asset agree (counted, not listed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReconcilePlan {
    pub drop: Vec<String>,
    pub add: Vec<String>,
    pub keep: usize,
}

impl ReconcilePlan {
    /// Render the plan for the consolidation prompt — the model sees
    /// exactly what the engine already decided.
    pub fn render(&self) -> String {
        let list = |v: &[String]| {
            if v.is_empty() {
                "(none)".to_string()
            } else {
                v.join(", ")
            }
        };
        format!(
            "- drop (stale pointers): {}\n- add (untracked topics): {}\n- keep: {} pointer(s)",
            list(&self.drop),
            list(&self.add),
            self.keep
        )
    }
}

/// Classify the index against the directory. `index_text` is passed in so
/// callers can reconcile a proposed index as well as the live one.
pub fn reconcile(dir: &Path, index_text: &str) -> ReconcilePlan {
    let mut named: Vec<String> = Vec::new();
    for line in index_text.lines() {
        // Proposals are never part of the trusted index (RT-3).
        if is_proposal_pointer(line) {
            continue;
        }
        if let Some(n) = topic_name(line) {
            named.push(n.to_string());
        }
    }
    named.sort();
    named.dedup();

    let mut plan = ReconcilePlan::default();
    for n in &named {
        let (topic, mtime) = topic_of(dir, n);
        let live = match topic {
            Topic::Missing => false,
            Topic::Bare(t) => entry_valid(&t, mtime),
            Topic::Headed(meta, t) => meta_current(&meta, &t, mtime),
            Topic::Malformed => false,
        };
        if live {
            plan.keep += 1;
        } else {
            plan.drop.push(n.clone());
        }
    }

    // Untracked topics: a file on disk no pointer names. Root and the layer
    // subdirs both count (pointers are always bare file names).
    let mut dirs = vec![dir.to_path_buf()];
    for layer in Layer::ALL {
        dirs.push(dir.join(layer.name()));
    }
    for d in dirs {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") || name == INDEX_NAME || name == CORE_NAME {
                continue;
            }
            if !named.iter().any(|n| n == &name) {
                plan.add.push(name);
            }
        }
    }
    plan.add.sort();
    plan.add.dedup();
    plan
}

/// Minimum confidence for an episodic entry to qualify for promotion to
/// semantic memory: settled is not enough, the entry must also be sure.
const PROMOTE_MIN_CONFIDENCE: f64 = 0.8;
/// Cap on promotion candidates per consolidation pass: the PROMOTE prompt
/// section must stay a bounded pointer list, not a second topic dump.
const PROMOTE_MAX: usize = 20;
/// Settling age: an episodic entry counts as settled when its clock is
/// older than 30 days — recent events are still being written, not
/// distilled.
const PROMOTE_AGE_SECS: u64 = 30 * 86_400;

/// Episodic→semantic promotion candidates: settled, high-confidence,
/// still-current episodic entries the consolidation model may distill
/// into durable facts.
///
/// Promotion is an index pointer add, never a body rewrite: the model
/// adds a `semantic/` pointer line to the new index while the episodic
/// file stays on disk untouched (bodies are append-only — see
/// `invalidate`). Qualification (every clause must hold):
///
/// - the file lives directly under `episodic/` and ends in `.md`;
/// - `entry_valid` (header parses, `valid_to`/TTL not elapsed);
/// - not superseded (`superseded_by` trailer absent — part of
///   `entry_valid`, listed because a superseded event must never be
///   re-distilled as fact);
/// - `confidence >= 0.8`;
/// - settled: the file's mtime is older than 30 days, or `valid_from`
///   is older than 30 days (either clock suffices — a backfilled event
///   carries an old `valid_from` on a freshly written file).
///
/// Sorted ascending and capped at 20, so the prompt section is
/// deterministic and bounded. Missing `episodic/` dir → empty (a fresh
/// memory has nothing to promote, which is not an error).
pub fn promote_candidates(dir: &Path) -> Vec<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let settled = |secs: u64| secs.saturating_add(PROMOTE_AGE_SECS) < now;
    let epi = dir.join(Layer::Episodic.name());
    let Ok(entries) = std::fs::read_dir(&epi) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        let p = epi.join(&name);
        if !p.is_file() {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&p) else {
            continue;
        };
        let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
        if !entry_valid(&text, mtime) {
            continue;
        }
        let Ok((meta, _)) = parse_meta(&text) else {
            continue; // Unreachable (entry_valid just parsed) — fail closed.
        };
        if meta.confidence < PROMOTE_MIN_CONFIDENCE {
            continue;
        }
        let mtime_settled = mtime
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .is_some_and(|d| settled(d.as_secs()));
        let from_settled = meta
            .valid_from
            .as_deref()
            .and_then(rfc3339_epoch)
            .is_some_and(settled);
        if !(mtime_settled || from_settled) {
            continue;
        }
        out.push(name);
    }
    out.sort();
    out.truncate(PROMOTE_MAX);
    out
}

/// Parse an RFC3339 timestamp to Unix epoch seconds (`None` on any
/// malformed input — callers treat unparseable as "not settled", never
/// as settled). Zero-dep companion to `format_utc_stamp`: days-from-civil
/// in reverse (Howard Hinnant's algorithm), with the numeric zone offset
/// applied. A leap second (`:60`) reads as `:59` — a one-second slop far
/// below the 30-day promotion bar. Inputs here already passed
/// `valid_rfc3339` via `parse_meta`, so this re-checks ranges lightly.
fn rfc3339_epoch(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    let num = |o: usize, n: usize| -> Option<i64> {
        let mut v = 0i64;
        for k in 0..n {
            let c = *b.get(o + k)?;
            if !c.is_ascii_digit() {
                return None;
            }
            v = v * 10 + i64::from(c - b'0');
        }
        Some(v)
    };
    let at = |o: usize, c: u8| -> bool { b.get(o) == Some(&c) };
    if b.len() < 20 {
        return None;
    }
    let y = num(0, 4)?;
    if !at(4, b'-') {
        return None;
    }
    let mo = num(5, 2)?;
    if !at(7, b'-') {
        return None;
    }
    let d = num(8, 2)?;
    if !at(10, b'T') && !at(10, b't') {
        return None;
    }
    let h = num(11, 2)?;
    if !at(13, b':') {
        return None;
    }
    let mi = num(14, 2)?;
    if !at(16, b':') {
        return None;
    }
    let se = num(17, 2)?;
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(|c| c.is_ascii_digit()) {
            i += 1;
        }
        if i == start {
            return None;
        }
    }
    let mut off_secs = 0i64;
    if b.get(i) == Some(&b'Z') || b.get(i) == Some(&b'z') {
        i += 1;
    } else if b.get(i) == Some(&b'+') || b.get(i) == Some(&b'-') {
        let sign = if b.get(i) == Some(&b'-') { -1 } else { 1 };
        let th = num(i + 1, 2)?;
        if !at(i + 3, b':') {
            return None;
        }
        let tm = num(i + 4, 2)?;
        if th > 23 || tm > 59 {
            return None;
        }
        off_secs = sign * (th * 3_600 + tm * 60);
        i += 6;
    } else {
        return None;
    }
    if i != b.len() {
        return None;
    }
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) {
        return None;
    }
    if h > 23 || mi > 59 || se > 60 {
        return None;
    }
    let yp = if mo <= 2 { y - 1 } else { y };
    let era = if yp >= 0 { yp } else { yp - 399 } / 400;
    let yoe = yp - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let stamp = days * 86_400 + h * 3_600 + mi * 60 + se.min(59) - off_secs;
    u64::try_from(stamp).ok()
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
            let is_topic = p.extension().is_some_and(|x| x == "md")
                && e.file_name() != INDEX_NAME
                && e.file_name() != CORE_NAME;
            if is_topic {
                // P8-B (zep validity, entry half): an expired or superseded
                // body must not be re-summarized into the index — consolidation
                // reads only what the reader would serve.
                let mtime = std::fs::metadata(&p).and_then(|m| m.modified()).ok();
                let Ok(t) = std::fs::read_to_string(&p) else {
                    continue;
                };
                if !entry_valid(&t, mtime) {
                    continue;
                }
                {
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

    // P8-B (mem0 reconcile): a deterministic plan over pointers vs backing
    // files, computed BEFORE the model call. It rides the prompt (so the
    // model sees exactly what changed) and is enforced afterwards (so a
    // stale pointer cannot survive a lazy reply).
    let plan = reconcile(dir, &old_index);
    // Episodic→semantic promotion: settled, high-confidence, still-current
    // episodic entries the model may distill into durable facts. Rides the
    // prompt as a candidate list only — the engine enforces ADD-only
    // afterwards (a promotion only ADDS a semantic/ pointer line; episodic
    // bodies are never moved, rewritten, or deleted).
    let candidates = promote_candidates(dir);
    let promote = if candidates.is_empty() {
        "PROMOTE (episodic→semantic candidates): (none — no settled \
         high-confidence episodic entries this pass)"
            .to_string()
    } else {
        format!(
            "PROMOTE (episodic→semantic candidates — you MAY promote these \
             episodic entries to semantic facts by ADDING a `semantic/` \
             pointer line per promoted fact; never move, rewrite, or delete \
             the episodic bodies): {}",
            candidates.join(", ")
        )
    };
    let prompt = format!(
        "You are consolidating an agent's file-based memory. Below is \
         INDEX.md (one-line pointers) and the heads of the topic files.\n\
         Reconcile plan (computed deterministically — honor it):\n{}\n\
         {promote}\n\
         Rewrite INDEX.md only: dedupe pointers, drop stale entries whose \
         topic file is gone, keep one line per topic in the form \
         `name.md — what it's about`. Validity: if a topic's content says \
         it expired or was superseded, drop its pointer.\n\
         ADD-only rules (no destructive rewrites): only append deltas or \
         drop stale pointers — never rewrite a topic file smaller and \
         never overwrite a quarantined entry; mark superseded entries \
         with a `superseded_by` trailer instead of deleting them; keep \
         entries whose validity window still covers now. Promotion is \
         ADD-only too: promoting an episodic entry only ADDS a semantic/ \
         pointer line — the episodic file stays on disk untouched.\n\
         Reply with the full new index between ---INDEX--- markers.\n\n\
         == CURRENT INDEX.md ==\n{old_index}\n== TOPIC HEADS =={topics}",
        plan.render()
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
    let mut capped: String = new_index.chars().take(INDEX_CAP).collect();
    // mem0 reconcile enforcement: pointers the plan marked stale never
    // come back, whatever the model replied (ADD-only: this can only drop
    // a pointer whose backing file is gone or whose asset expired).
    let mut dropped_stale = 0usize;
    if !plan.drop.is_empty() {
        let kept: Vec<&str> = capped
            .lines()
            .filter(|l| {
                let stale = plan
                    .drop
                    .iter()
                    .any(|d| topic_name(l).is_some_and(|n| n == d.as_str()));
                if stale {
                    dropped_stale += 1;
                }
                !stale
            })
            .collect();
        capped = kept.join("\n");
    }
    std::fs::write(&idx, format!("{capped}\n")).map_err(|e| e.to_string())?;
    commit(dir, "consolidate");

    let dropped = old_index
        .lines()
        .filter(|l| l.contains(".md"))
        .filter(|l| !new_index.contains(l.trim()))
        .count();
    Ok(format!(
        "consolidated: {} → {} index lines, {dropped} pointers dropped \
         (reconcile: {} stale, {} untracked, {dropped_stale} stale reclaimed)",
        old_index.lines().count(),
        capped.lines().count(),
        plan.drop.len(),
        plan.add.len()
    ))
}

/// Cap on `search_memory` hits. A search answers one question; it must not
/// stream a whole memory dir into a prompt, so a larger `limit` is clamped
/// (deliberately — this is a prompt budget, not a pagination knob).
pub const MAX_SEARCH_HITS: usize = 50;

/// Weight of a query term appearing in the topic file's name. A name hit
/// says what the file is *about*; a body hit is one mention among many.
const NAME_WEIGHT: u32 = 3;
/// Ceiling on one term's body occurrences: a repeated word must not swamp
/// a name hit (repetition is one fact stated many times).
const BODY_CAP: usize = 5;
/// Cap on a snippet's length in chars, the trailing `…` included.
const SNIPPET_CHARS: usize = 200;

/// LanceDB frontmatter prefilter (the `WHERE` half of a vector search): the
/// header columns a caller may constrain, applied *before* ranking so a
/// filtered-out asset can never occupy a result slot. Every clause is
/// fail-closed — a column that cannot be proven excludes the asset.
#[derive(Debug, Clone, PartialEq)]
pub struct EntryFilter {
    /// Exact layer (directory) match; `None` = every layer.
    pub layer: Option<Layer>,
    /// Sensitivity ceiling, checked through `admits`; `None` = no ceiling.
    pub sensitivity_max: Option<Sensitivity>,
    /// The subagent-view rule: a `Regulated` asset never fans out, whatever
    /// its sensitivity tier. Defaults to **true** (see `Default`).
    pub exclude_regulated: bool,
    /// Inclusive minimum header confidence. A non-finite entry confidence
    /// never matches: `validate_meta` cannot produce one, a directly
    /// constructed `EntryMeta` can, and an unreadable number must not pass
    /// a numeric gate.
    pub min_confidence: Option<f64>,
    /// Exact provenance match, case-insensitive. Not a substring match:
    /// `"seed"` does not admit `"seed-notes"`.
    pub provenance: Option<String>,
}

impl Default for EntryFilter {
    /// Deliberately hand-written, not derived: `bool`'s derived default is
    /// `false`, which would fan *regulated* assets out to whoever forgot to
    /// set the column. The subagent-view rule is the safe default, so the
    /// column starts at `true`.
    fn default() -> Self {
        EntryFilter {
            layer: None,
            sensitivity_max: None,
            exclude_regulated: true,
            min_confidence: None,
            provenance: None,
        }
    }
}

/// True when `meta` survives `f` for a file in `layer`: the column checks a
/// vector store would have run as SQL, decided on the header alone (no body
/// text, no ranking, no fs). Layer is equality; sensitivity goes through
/// the one `admits` ordering; confidence is inclusive and must be finite;
/// provenance is an exact case-insensitive compare.
pub fn matches_filter(meta: &EntryMeta, layer: Layer, f: &EntryFilter) -> bool {
    if let Some(want) = f.layer {
        if layer != want {
            return false;
        }
    }
    if let Some(max) = f.sensitivity_max {
        if !admits(max, meta.sensitivity) {
            return false;
        }
    }
    if f.exclude_regulated && meta.governance == Governance::Regulated {
        return false;
    }
    if let Some(min) = f.min_confidence {
        if !meta.confidence.is_finite() || meta.confidence < min {
            return false;
        }
    }
    if let Some(want) = &f.provenance {
        if !meta.provenance.eq_ignore_ascii_case(want) {
            return false;
        }
    }
    true
}

/// Resolve one topic name for filter/search: the layer dir that files it and
/// the path to read, or `None` when no backing file exists. The layer-dir
/// copy wins over a root-level duplicate — the filed asset is the one the
/// layer column judges, and both callers must resolve it the same way.
///
/// A root-level topic (the pre-layer `facts.md` files) is filed as
/// `Layer::Semantic`: it is world-fact-shaped, and an unscoped name must
/// never be judged as identity (`Profile`) or personal history
/// (`Episodic`), the two layers with a higher write bar.
fn filed_topic(dir: &Path, name: &str) -> Option<(Layer, PathBuf)> {
    for layer in Layer::ALL {
        let p = dir.join(layer.name()).join(name);
        if p.is_file() {
            return Some((layer, p));
        }
    }
    let root = dir.join(name);
    root.is_file().then_some((Layer::Semantic, root))
}

/// Distinct topic-file names the filter admits, ascending: memory root plus
/// the four layer dirs, minus the two non-topic `.md` files (`INDEX.md`,
/// `CORE.md`). Still-current only (`entry_valid`'s rule: header parses and
/// is neither expired nor superseded) and matching `f` — this is the
/// prefilter `search_memory` ranks, so an asset dropped here can never be
/// scored, let alone returned.
pub fn filter_entries(dir: &Path, f: &EntryFilter) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut dirs: Vec<PathBuf> = vec![dir.to_path_buf()];
    for layer in Layer::ALL {
        dirs.push(dir.join(layer.name()));
    }
    for d in dirs {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") || name == INDEX_NAME || name == CORE_NAME {
                continue;
            }
            if e.path().is_file() {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    names.retain(|name| {
        let Some((layer, path)) = filed_topic(dir, name) else {
            return false;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return false;
        };
        // Unparsable header: fail closed (never served as current).
        let Ok((meta, _)) = parse_meta(&text) else {
            return false;
        };
        let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        meta_current(&meta, &text, mtime) && matches_filter(&meta, layer, f)
    });
    names
}

/// One ranked hit from `search_memory` (mcp-mem-server `search_memory`):
/// which topic file matched, its layer, the lexical score, and the quotable
/// line — the caller can re-read `name` at `line` instead of trusting the
/// snippet.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub name: String,
    pub layer: Layer,
    pub score: u32,
    /// 1-based line number of `snippet` in the file; 0 when the file has no
    /// body line (never a fabricated line 1).
    pub line: usize,
    pub snippet: String,
}

/// Lexical search over the live topic files (mcp-mem-server `search_memory`):
/// score the *filtered* assets, best first, and return at most `limit` of
/// them. Deterministic by construction — the ordering is score descending,
/// then name ascending, so the same dir and query always give the same list.
///
/// Contract:
/// - the query is trimmed; no alphanumeric term after trimming is an error
///   (a search with no terms matches nothing);
/// - `limit` must be positive; anything above `MAX_SEARCH_HITS` is clamped
///   to it (a search must not stream a whole memory dir);
/// - a term in the file's name scores `NAME_WEIGHT`, each body occurrence 1
///   up to `BODY_CAP` per term per file, and a file carrying no term at all
///   is not a hit;
/// - `filter` runs first (`filter_entries`), so superseded/expired files,
///   unparsable headers, and everything the filter rejects are never
///   ranked and never occupy a slot.
pub fn search_memory(
    dir: &Path,
    query: &str,
    filter: &EntryFilter,
    limit: usize,
) -> Result<Vec<MemoryHit>, String> {
    let terms = query_tokens(query);
    if terms.is_empty() {
        return Err(
            "memory: search_memory: a search with no terms matches nothing — pass at \
             least one alphanumeric word"
                .into(),
        );
    }
    if limit == 0 {
        return Err(
            "memory: search_memory: limit 0 would return nothing — pass a positive limit".into(),
        );
    }
    let limit = limit.min(MAX_SEARCH_HITS);

    let mut hits: Vec<MemoryHit> = Vec::new();
    for name in filter_entries(dir, filter) {
        let Some((layer, path)) = filed_topic(dir, &name) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        // Fail closed: a header that stopped parsing between the prefilter
        // and the read is not a hit.
        let Ok((_, body)) = parse_meta(&text) else {
            continue;
        };
        let stem = name.strip_suffix(".md").unwrap_or(&name);
        let name_words = word_tokens(stem);
        let body_words = word_tokens(&body);
        let mut score = 0u32;
        // Best term wins the snippet; equal scores keep the earlier term, so
        // a repeated query is not a way to change the answer.
        let mut best: Option<(usize, u32)> = None;
        for (i, term) in terms.iter().enumerate() {
            let weight = NAME_WEIGHT * u32::from(name_words.iter().any(|w| w == term))
                + occurrences(&body_words, term) as u32;
            score += weight;
            if weight > 0 && best.is_none_or(|(_, w)| weight > w) {
                best = Some((i, weight));
            }
        }
        if score == 0 {
            continue;
        }
        // Frontmatter lines sit above the body, so a body line's number is
        // its index plus this head — `line` is a real file line.
        let head = text.lines().count().saturating_sub(body.lines().count());
        let (snippet, line) = match best {
            Some((i, _)) => snippet_for(&body, &terms[i], head),
            None => (String::new(), 0),
        };
        hits.push(MemoryHit {
            name,
            layer,
            score,
            line,
            snippet,
        });
    }
    hits.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.name.cmp(&b.name)));
    hits.truncate(limit);
    Ok(hits)
}

/// Lowercased alphanumeric words — the module's one tokenizer (`""` splits
/// into nothing, so punctuation and separators fall out by construction).
fn word_tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect()
}

/// Query terms: lowercased alphanumeric words, deduplicated, in the order
/// the caller wrote them (scoring adds either way, but the snippet tie-break
/// must be reproducible).
fn query_tokens(query: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in word_tokens(query) {
        if !out.contains(&t) {
            out.push(t);
        }
    }
    out
}

/// Occurrences of `term` as a whole word in `words`, capped at `BODY_CAP` —
/// `"cat"` matches the word `cat`, never `concatenate`.
fn occurrences(words: &[String], term: &str) -> usize {
    words
        .iter()
        .filter(|w| w.as_str() == term)
        .count()
        .min(BODY_CAP)
}

/// `s` cut to at most `max` chars (char counts, never byte offsets — a
/// multibyte char is never split); the cut is marked with a trailing `…`
/// that counts toward the budget.
fn cut_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// The quotable line for one hit: the trimmed first body line carrying
/// `term` (the highest-scoring query term), cut to `SNIPPET_CHARS`. A
/// name-only hit has no such line, so it falls back to the first non-empty
/// body line — still a line the caller can read. `head` is the number of
/// frontmatter lines above the body, so the returned number is the line's
/// real 1-based position in the file; `(String::new(), 0)` means the file
/// has no body line at all.
fn snippet_for(body: &str, term: &str, head: usize) -> (String, usize) {
    let lines: Vec<&str> = body.lines().collect();
    let mut fallback: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if fallback.is_none() {
            fallback = Some(i);
        }
        if word_tokens(trimmed).iter().any(|w| w == term) {
            return (cut_chars(trimmed, SNIPPET_CHARS), head + i + 1);
        }
    }
    match fallback {
        Some(i) => (cut_chars(lines[i].trim(), SNIPPET_CHARS), head + i + 1),
        None => (String::new(), 0),
    }
}

/// mem0 additive fusion (`score_and_rank` inner loop, arsenal B2): gate
/// `sem` on `threshold` first, then average the present signals. The
/// caller supplies `[0,1]` signals; the divisor is the max possible
/// mass — sem-only 1.0, +keyword 2.0, +entity 2.5, sem+entity 1.5 (the
/// entity boost caps at 0.5, see `entity_link`, so full house sums to
/// 2.5). Fail-closed: a gated or non-finite input fuses to 0.0; the
/// result clamps to 0..1.
pub fn fuse_scores(
    sem: f32,
    kw: f32,
    ent: f32,
    has_kw: bool,
    has_ent: bool,
    threshold: f32,
) -> f32 {
    if !sem.is_finite() || !kw.is_finite() || !ent.is_finite() || !threshold.is_finite() {
        return 0.0;
    }
    if sem < threshold {
        return 0.0;
    }
    let denom = match (has_kw, has_ent) {
        (false, false) => 1.0,
        (true, false) => 2.0,
        (true, true) => 2.5,
        (false, true) => 1.5,
    };
    let num = sem + if has_kw { kw } else { 0.0 } + if has_ent { ent } else { 0.0 };
    (num / denom).clamp(0.0, 1.0)
}

/// mem0 BM25 sigmoid (`get_bm25_params`/`normalize_bm25`): the
/// (midpoint, steepness) pair is picked by term count clamped to the
/// table ends — a one-term query saturates early, a long one late.
/// No lemmatizer here by design: the caller passes the raw BM25 sum
/// and the pre-lemmatization term count.
pub fn normalize_keyword(raw: f64, n_terms: u64) -> f64 {
    if !raw.is_finite() {
        return 0.0;
    }
    const TABLE: [(f64, f64); 5] = [(5.0, 0.7), (7.0, 0.6), (9.0, 0.5), (10.0, 0.5), (12.0, 0.5)];
    let idx = n_terms.clamp(1, TABLE.len() as u64) as usize - 1;
    let (mid, steep) = TABLE[idx];
    1.0 / (1.0 + (-steep * (raw - mid)).exp())
}

/// mem0 entity boost (v3 `_upsert_entity`): an exact normalized match
/// pays the full 0.5; otherwise the best per-entity token-overlap
/// fraction (covered entity tokens over entity tokens) pays
/// proportionally. Capped at 0.5 — the fusion divisor (2.5) already
/// reserves exactly that mass, so a boost above it would double-count.
pub fn entity_link(memories: &[&str], entities: &[&str]) -> Vec<f32> {
    use std::collections::BTreeSet;
    let ent_norm: Vec<String> = entities.iter().map(|e| word_tokens(e).join(" ")).collect();
    let ent_sets: Vec<BTreeSet<String>> = entities
        .iter()
        .map(|e| word_tokens(e).into_iter().collect())
        .collect();
    memories
        .iter()
        .map(|m| {
            let norm = word_tokens(m).join(" ");
            if !norm.is_empty() && ent_norm.iter().any(|e| e == &norm) {
                return 0.5;
            }
            let mem_set: BTreeSet<String> = word_tokens(m).into_iter().collect();
            if mem_set.is_empty() {
                return 0.0;
            }
            let best = ent_sets
                .iter()
                .filter(|s| !s.is_empty())
                .map(|s| s.intersection(&mem_set).count() as f32 / s.len() as f32)
                .fold(0.0f32, f32::max);
            (best * 0.5).min(0.5)
        })
        .collect()
}

/// cognee content-hash dedup (the `(dataset,owner,content_hash)` index):
/// sha256 hex of the body bytes. The hash covers content only — the
/// caller scopes dataset/owner by which entries it passes to
/// `dedupe_by_hash`.
pub fn content_hash(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

/// First-seen wins by `content_hash` of the body: the earliest entry
/// with a hash keeps its name in `unique`, later collisions land in
/// `dupes`. Both outputs sorted ascending, so the verdict is
/// deterministic regardless of input order ties.
pub fn dedupe_by_hash(entries: &[(String, String)]) -> (Vec<String>, Vec<String>) {
    use std::collections::BTreeSet;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    let mut unique = Vec::new();
    let mut dupes = Vec::new();
    for (name, body) in entries {
        if seen.insert(content_hash(body)) {
            unique.push(name.clone());
        } else {
            dupes.push(name.clone());
        }
    }
    unique.sort();
    dupes.sort();
    (unique, dupes)
}

/// letta P1: the `core_block` cap as a reusable pure fn. Under budget
/// the (trimmed) text rides with no note; over budget the head is cut
/// on a char boundary (a multibyte char is never split) and rides with
/// a repair note — never a silent cut.
pub fn core_budget(text: &str, cap: usize) -> (String, Option<String>) {
    let text = text.trim();
    if text.len() <= cap {
        return (text.to_string(), None);
    }
    let cut = text
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|end| *end <= cap)
        .last()
        .unwrap_or(0);
    (
        text[..cut].to_string(),
        Some(format!(
            "[overseer] {CORE_NAME} exceeds {cap} bytes — keep it to the lines \
             that must survive every compaction."
        )),
    )
}

/// letta archival paging over `filter_entries` output: sorted ascending
/// for determinism, sliced from `cursor`, taking `page_len`. The next
/// cursor is `Some` only while names remain; a zero page or a cursor
/// past the end yields empty with no continuation.
pub fn archive_page(
    names: &[String],
    cursor: usize,
    page_len: usize,
) -> (Vec<String>, Option<usize>) {
    let mut sorted: Vec<String> = names.to_vec();
    sorted.sort();
    if page_len == 0 || cursor >= sorted.len() {
        return (Vec::new(), None);
    }
    let end = (cursor + page_len).min(sorted.len());
    let page = sorted[cursor..end].to_vec();
    let next = if end < sorted.len() { Some(end) } else { None };
    (page, next)
}

/// cognee `improve` proposal renderer (P4): a line diff between the
/// stored note and the session note — dropped lines (`- drop:`) in old
/// order, added lines (`+ add:`) in new order, multiset-aware so a
/// repeated line dropped once reports once. Render only: the model
/// decides, the engine writes. Empty when the notes agree.
pub fn improve_note(old: &str, new: &str) -> String {
    use std::collections::BTreeMap;
    let lines = |s: &str| -> Vec<String> {
        s.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect()
    };
    let old_lines = lines(old);
    let new_lines = lines(new);
    let mut avail: BTreeMap<&str, usize> = BTreeMap::new();
    for l in &new_lines {
        *avail.entry(l.as_str()).or_insert(0) += 1;
    }
    let mut out = Vec::new();
    for l in &old_lines {
        match avail.get_mut(l.as_str()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(format!("- drop: {l}")),
        }
    }
    avail.clear();
    for l in &old_lines {
        *avail.entry(l.as_str()).or_insert(0) += 1;
    }
    for l in &new_lines {
        match avail.get_mut(l.as_str()) {
            Some(n) if *n > 0 => *n -= 1,
            _ => out.push(format!("+ add: {l}")),
        }
    }
    out.join("\n")
}

/// MiMo phrase-OR CJK tokenizer (`fts-query.ts`): lowercase word tokens
/// for alphanumeric runs, one token per CJK char (no word segmentation
/// — each ideograph matches alone), deduped and sorted so the term
/// list is deterministic.
pub fn tokenize_fts(query: &str) -> Vec<String> {
    use std::collections::BTreeSet;
    fn is_cjk(c: char) -> bool {
        matches!(
            c as u32,
            0x3400..=0x4DBF
                | 0x4E00..=0x9FFF
                | 0xF900..=0xFAFF
                | 0x3040..=0x30FF
                | 0xAC00..=0xD7AF
                | 0x20000..=0x2EBE0
        )
    }
    let mut terms: BTreeSet<String> = BTreeSet::new();
    let mut buf = String::new();
    for c in query.chars() {
        if is_cjk(c) {
            if !buf.is_empty() {
                terms.insert(std::mem::take(&mut buf));
            }
            terms.insert(c.to_lowercase().collect::<String>());
        } else if c.is_alphanumeric() {
            for l in c.to_lowercase() {
                buf.push(l);
            }
        } else if !buf.is_empty() {
            terms.insert(std::mem::take(&mut buf));
        }
    }
    if !buf.is_empty() {
        terms.insert(buf);
    }
    terms.into_iter().collect()
}

/// MiMo BM25 floor (`service.ts`): term-hit fraction over the
/// lowercased text, substring (phrase-OR) per term. A non-empty term
/// list never scores below 0.05 — an FTS miss is weak evidence, not
/// disproof; an empty term list matches nothing (0.0).
pub fn fts_score(terms: &[&str], text: &str) -> f32 {
    let terms: Vec<&str> = terms.iter().copied().filter(|t| !t.is_empty()).collect();
    if terms.is_empty() {
        return 0.0;
    }
    let lowered = text.to_lowercase();
    let hits = terms
        .iter()
        .filter(|t| lowered.contains(&t.to_lowercase()))
        .count();
    ((hits as f32) / (terms.len() as f32)).max(0.05)
}

/// MiMo fingerprint reconcile: lowercase alnum-only collapse, then
/// `content_hash`. Bodies differing only in case/punctuation share a
/// fingerprint; anything else differs.
pub fn fingerprint(text: &str) -> String {
    let collapsed: String = text
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect();
    content_hash(&collapsed)
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
    fn reconcile_classifies_pointers_against_the_directory() {
        // P8-B accept (mem0 reconcile): drop stale pointers, add untracked
        // topics, count the ones that agree — all before any model call.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        std::fs::write(dir.join("semantic").join("live.md"), "body\n").unwrap();
        std::fs::write(
            dir.join("semantic").join("gone.md"),
            "---\nvalid_to: 2000-01-01T00:00:00Z\n---\nstale\n",
        )
        .unwrap();
        std::fs::write(dir.join("untracked.md"), "not pointed at\n").unwrap();
        std::fs::write(dir.join(CORE_NAME), "core lines\n").unwrap();
        let index = "# Memory Index\n\nlive.md — fine\ngone.md — expired\nmissing.md — nowhere\n";

        let plan = reconcile(&dir, index);
        assert_eq!(plan.keep, 1, "live.md agrees");
        assert_eq!(
            plan.drop,
            vec!["gone.md".to_string(), "missing.md".to_string()]
        );
        assert_eq!(plan.add, vec!["untracked.md".to_string()]);
        // CORE.md is never a topic pointer target.
        assert!(!plan.add.iter().any(|a| a == CORE_NAME));
        // The rendered plan rides the consolidation prompt verbatim.
        let rendered = plan.render();
        assert!(rendered.contains("gone.md"), "{rendered}");
        assert!(rendered.contains("untracked.md"), "{rendered}");
        assert!(rendered.contains("keep: 1"), "{rendered}");

        // An empty plan renders "(none)" rather than an empty line.
        let empty = reconcile(&dir, "");
        assert!(empty.render().contains("(none)"), "{}", empty.render());
    }

    #[test]
    fn consolidate_enforces_the_reconcile_plan_and_skips_stale_bodies() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        std::fs::write(dir.join("facts.md"), "live facts\n").unwrap();
        std::fs::write(
            dir.join("dead.md"),
            "---\nvalid_to: 2000-01-01T00:00:00Z\n---\nSTALE_BODY_MARKER\n",
        )
        .unwrap();
        std::fs::write(
            dir.join(INDEX_NAME),
            "# Memory Index\n\nfacts.md — facts\ndead.md — expired\n",
        )
        .unwrap();

        // The model lazily keeps the stale pointer and drops the live one.
        let p = FixedProvider::with_reply(
            "---INDEX---\n# Memory Index\n\ndead.md — expired\nfacts.md — live facts\n---INDEX---",
        );
        let msg = consolidate(&p, "tiny", &dir).unwrap();
        assert!(msg.contains("1 stale reclaimed"), "{msg}");
        let new = std::fs::read_to_string(dir.join(INDEX_NAME)).unwrap();
        assert!(
            !new.contains("dead.md"),
            "the engine reclaims a stale pointer the model kept: {new}"
        );
        assert!(new.contains("facts.md"), "{new}");

        // The stale body never reached the prompt; the plan did.
        let seen = p.seen.lock().map(|g| g.join("\n")).unwrap_or_default();
        assert!(!seen.contains("STALE_BODY_MARKER"), "stale body was served");
        assert!(seen.contains("Reconcile plan"), "{seen}");
        assert!(seen.contains("dead.md"), "the plan names the stale pointer");
    }

    #[test]
    fn core_block_is_resident_capped_and_parent_only() {
        // P8-B accept (letta CORE.md): a small always-in-context block,
        // capped with a repair note, and absent from the subagent view.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        std::fs::write(dir.join(INDEX_NAME), "# Memory Index\n\nfacts.md — facts\n").unwrap();
        std::fs::write(dir.join("facts.md"), "body\n").unwrap();

        // Absent → no block at all (opt-in by existence).
        let seg = index_segment(&dir);
        assert!(!seg.contains("Memory core"), "{seg}");

        std::fs::write(dir.join(CORE_NAME), "Never page me out.\n").unwrap();
        let seg = index_segment(&dir);
        assert!(seg.contains("## Memory core (CORE.md)"), "{seg}");
        assert!(seg.contains("Never page me out."));
        // Routing hint (lightrag): both halves of the local/global split.
        assert!(seg.contains("answer specific questions from"), "{seg}");
        assert!(seg.contains("overview questions from this index"), "{seg}");

        // Over the cap → truncated with the repair note, and the bytes stay
        // bounded (the block is resident, so it must not grow unbounded).
        std::fs::write(dir.join(CORE_NAME), "y".repeat(CORE_CAP + 500)).unwrap();
        let seg = index_segment(&dir);
        assert!(seg.contains("exceeds"), "{seg}");
        assert!(seg.len() < INDEX_CAP + CORE_CAP + 2_000, "{}", seg.len());

        // CORE.md is not a topic: it never appears as a pointer, and the
        // quarantined subagent view carries neither the block nor a pointer.
        std::fs::write(dir.join(CORE_NAME), "core\n").unwrap();
        let filtered = index_segment_filtered(&dir, Sensitivity::Personal);
        assert!(!filtered.contains("Memory core"), "{filtered}");
        assert!(!filtered.contains("CORE.md"), "{filtered}");
        // CORE.md must never be reconciled as a topic file.
        let plan = reconcile(&dir, "# Memory Index\n\nfacts.md — facts\n");
        assert!(!plan.add.iter().any(|a| a == CORE_NAME));
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

    /// Write one topic file, creating the layer dir when needed.
    fn put(dir: &Path, rel: &str, text: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, text).unwrap();
    }

    fn search(dir: &Path, q: &str, f: &EntryFilter) -> Vec<MemoryHit> {
        search_memory(dir, q, f, 10).unwrap()
    }

    fn names(hits: &[MemoryHit]) -> Vec<&str> {
        hits.iter().map(|h| h.name.as_str()).collect()
    }

    #[test]
    fn search_weights_name_hits_above_body_only_hits() {
        let dir = tmpdir();
        put(&dir, "semantic/other.md", "the kettle boils slowly\n");
        put(&dir, "semantic/kettle.md", "no query word in this body\n");
        let hits = search(&dir, "  Kettle  ", &EntryFilter::default());
        assert_eq!(names(&hits), ["kettle.md", "other.md"]);
        assert_eq!(hits[0].score, NAME_WEIGHT, "a name hit is worth 3");
        assert_eq!(hits[1].score, 1, "one body occurrence is worth 1");
    }

    #[test]
    fn search_excludes_expired_and_superseded_assets() {
        let dir = tmpdir();
        put(
            &dir,
            "semantic/expired.md",
            "---\nvalid_to: 2000-01-01T00:00:00Z\n---\nkettle kettle kettle\n",
        );
        put(
            &dir,
            "semantic/superseded.md",
            "kettle kettle\nsuperseded_by live.md\n",
        );
        put(&dir, "semantic/live.md", "kettle\n");
        let hits = search(&dir, "kettle", &EntryFilter::default());
        assert_eq!(
            names(&hits),
            ["live.md"],
            "stale assets are not ranked, let alone returned"
        );
    }

    #[test]
    fn search_honours_a_personal_ceiling_over_secret_entries() {
        let dir = tmpdir();
        // The secret twin scores highest (name hit + body hit) — only the
        // ceiling removes it.
        put(
            &dir,
            "semantic/kettle-secret.md",
            "---\nsensitivity: secret\n---\nkettle kettle\n",
        );
        put(&dir, "semantic/kettle-open.md", "one kettle mention\n");
        let ceiling = EntryFilter {
            sensitivity_max: Some(Sensitivity::Personal),
            ..EntryFilter::default()
        };
        assert_eq!(
            names(&search(&dir, "kettle", &ceiling)),
            ["kettle-open.md"],
            "a secret body is never served under a personal ceiling"
        );
        // Without a ceiling the secret twin is the top scorer, which is what
        // proves the ceiling — not the ranking — removed it.
        let open = search(&dir, "kettle", &EntryFilter::default());
        assert_eq!(open[0].name, "kettle-secret.md", "{open:?}");
    }

    #[test]
    fn search_never_returns_a_regulated_entry_even_when_it_scores_highest() {
        let dir = tmpdir();
        put(
            &dir,
            "semantic/kettle-regulated.md",
            "---\ngovernance: regulated\n---\nkettle kettle kettle kettle kettle kettle\n",
        );
        put(&dir, "semantic/kettle-notes.md", "one kettle mention\n");
        // With the governance column switched off the regulated asset is
        // rank 0: its text really does score highest.
        let ungoverned = EntryFilter {
            exclude_regulated: false,
            ..EntryFilter::default()
        };
        assert_eq!(
            search(&dir, "kettle", &ungoverned)[0].name,
            "kettle-regulated.md"
        );
        assert_eq!(
            names(&search(&dir, "kettle", &EntryFilter::default())),
            ["kettle-notes.md"],
            "the prefilter runs before ranking, so nothing regulated takes a slot"
        );
    }

    #[test]
    fn search_skips_malformed_headers() {
        let dir = tmpdir();
        put(
            &dir,
            "semantic/broken.md",
            "---\nconfidence: nonsense\n---\nkettle kettle kettle\n",
        );
        put(&dir, "semantic/fine.md", "kettle\n");
        assert_eq!(
            names(&search(&dir, "kettle", &EntryFilter::default())),
            ["fine.md"]
        );
        // The prefilter and the search read the same gate.
        assert_eq!(filter_entries(&dir, &EntryFilter::default()), ["fine.md"]);
    }

    #[test]
    fn search_ties_break_on_name_ascending() {
        let dir = tmpdir();
        for n in ["c.md", "a.md", "b.md"] {
            put(&dir, &format!("semantic/{n}"), "kettle\n");
        }
        let hits = search(&dir, "kettle", &EntryFilter::default());
        assert!(hits.iter().all(|h| h.score == 1));
        assert_eq!(names(&hits), ["a.md", "b.md", "c.md"], "{hits:?}");
        // Same dir, same query, same order — every run.
        assert_eq!(
            names(&hits),
            names(&search(&dir, "kettle", &EntryFilter::default()))
        );
    }

    #[test]
    fn search_rejects_a_query_with_no_terms() {
        let dir = tmpdir();
        put(&dir, "semantic/kettle.md", "kettle\n");
        for q in ["", "   ", " — ... "] {
            let e = search_memory(&dir, q, &EntryFilter::default(), 5).unwrap_err();
            assert!(e.contains("no terms"), "{e}");
            assert!(e.contains("matches nothing"), "{e}");
        }
    }

    #[test]
    fn search_clamps_limit_and_rejects_zero() {
        let dir = tmpdir();
        for i in 0..(MAX_SEARCH_HITS + 5) {
            put(&dir, &format!("semantic/n{i:03}.md"), "kettle\n");
        }
        assert_eq!(
            search_memory(&dir, "kettle", &EntryFilter::default(), 10_000)
                .unwrap()
                .len(),
            MAX_SEARCH_HITS,
            "a search must not stream the whole memory dir"
        );
        assert_eq!(
            search_memory(&dir, "kettle", &EntryFilter::default(), 3)
                .unwrap()
                .len(),
            3
        );
        let e = search_memory(&dir, "kettle", &EntryFilter::default(), 0).unwrap_err();
        assert!(e.contains("limit 0"), "{e}");
    }

    #[test]
    fn search_snippet_line_number_matches_the_file() {
        let dir = tmpdir();
        let text = "---\nconfidence: 0.9\n---\n# Notes\n\nfirst kettle line\nsecond line\n";
        put(&dir, "semantic/lines.md", text);
        let hits = search(&dir, "kettle", &EntryFilter::default());
        assert_eq!(hits[0].snippet, "first kettle line");
        assert_eq!(hits[0].line, 6);
        assert_eq!(
            text.lines().nth(hits[0].line - 1).unwrap().trim(),
            hits[0].snippet,
            "the line number must address the snippet in the file"
        );

        // A name-only hit has no matching body line: the first non-empty
        // body line is quoted, and its number is still the file's.
        put(
            &dir,
            "semantic/only-name.md",
            "---\nx: 1\n---\n\nfirst real line\nsecond\n",
        );
        let hits = search(&dir, "only", &EntryFilter::default());
        assert_eq!(names(&hits), ["only-name.md"]);
        assert_eq!(hits[0].snippet, "first real line");
        assert_eq!(hits[0].line, 5);

        // A long line is cut to the cap, and the cut is marked.
        put(
            &dir,
            "semantic/long.md",
            &format!("kettle {}\n", "w".repeat(500)),
        );
        let hits = search(&dir, "kettle", &EntryFilter::default());
        let long = hits
            .iter()
            .find(|h| h.name == "long.md")
            .unwrap_or_else(|| panic!("long.md must be a hit: {hits:?}"));
        assert_eq!(long.snippet.chars().count(), SNIPPET_CHARS);
        assert!(long.snippet.ends_with('…'), "{}", long.snippet);
    }

    #[test]
    fn filter_entries_is_sorted_distinct_and_current() {
        let dir = tmpdir();
        put(&dir, "semantic/zeta.md", "z\n");
        put(&dir, "profile/alpha.md", "a\n");
        put(&dir, "beta.md", "b\n");
        put(
            &dir,
            "expired.md",
            "---\nvalid_to: 2000-01-01T00:00:00Z\n---\nx\n",
        );
        put(&dir, INDEX_NAME, "index\n");
        put(&dir, CORE_NAME, "core\n");
        put(&dir, "semantic/notes.txt", "not a topic\n");
        assert_eq!(
            filter_entries(&dir, &EntryFilter::default()),
            ["alpha.md", "beta.md", "zeta.md"],
            "layer dirs and the root, minus INDEX/CORE, current only, sorted"
        );
        let profile = EntryFilter {
            layer: Some(Layer::Profile),
            ..EntryFilter::default()
        };
        assert_eq!(filter_entries(&dir, &profile), ["alpha.md"]);
        // A root-level topic is filed as Semantic (never as a higher-bar
        // layer), so a semantic column still reaches it.
        let semantic = EntryFilter {
            layer: Some(Layer::Semantic),
            ..EntryFilter::default()
        };
        assert_eq!(filter_entries(&dir, &semantic), ["beta.md", "zeta.md"]);
    }

    #[test]
    fn filter_default_is_default_deny_on_regulated() {
        let f = EntryFilter::default();
        assert!(
            f.exclude_regulated,
            "a derived default would fan regulated assets out"
        );
        let regulated = EntryMeta {
            governance: Governance::Regulated,
            ..EntryMeta::default()
        };
        assert!(!matches_filter(&regulated, Layer::Semantic, &f));
        assert!(matches_filter(&EntryMeta::default(), Layer::Semantic, &f));

        // Layer is equality; the sensitivity ceiling goes through `admits`.
        let profile = EntryFilter {
            layer: Some(Layer::Profile),
            ..EntryFilter::default()
        };
        assert!(matches_filter(
            &EntryMeta::default(),
            Layer::Profile,
            &profile
        ));
        assert!(!matches_filter(
            &EntryMeta::default(),
            Layer::Semantic,
            &profile
        ));
        let personal = EntryFilter {
            sensitivity_max: Some(Sensitivity::Personal),
            ..EntryFilter::default()
        };
        let public = EntryMeta {
            sensitivity: Sensitivity::Public,
            ..EntryMeta::default()
        };
        let secret = EntryMeta {
            sensitivity: Sensitivity::Secret,
            ..EntryMeta::default()
        };
        assert!(matches_filter(&public, Layer::Semantic, &personal));
        assert!(!matches_filter(&secret, Layer::Semantic, &personal));
    }

    #[test]
    fn filter_min_confidence_is_inclusive_and_rejects_non_finite() {
        let meta = EntryMeta {
            confidence: 0.5,
            ..EntryMeta::default()
        };
        let at = EntryFilter {
            min_confidence: Some(0.5),
            ..EntryFilter::default()
        };
        assert!(matches_filter(&meta, Layer::Semantic, &at), "inclusive");
        let above = EntryFilter {
            min_confidence: Some(0.51),
            ..EntryFilter::default()
        };
        assert!(!matches_filter(&meta, Layer::Semantic, &above));
        let nan = EntryMeta {
            confidence: f64::NAN,
            ..EntryMeta::default()
        };
        let any = EntryFilter {
            min_confidence: Some(0.0),
            ..EntryFilter::default()
        };
        assert!(
            !matches_filter(&nan, Layer::Semantic, &any),
            "an unreadable confidence never passes a numeric gate"
        );
    }

    #[test]
    fn filter_provenance_is_case_insensitive_but_exact() {
        let meta = EntryMeta {
            provenance: "Seed-Notes".into(),
            ..EntryMeta::default()
        };
        let same = EntryFilter {
            provenance: Some("seed-notes".into()),
            ..EntryFilter::default()
        };
        assert!(matches_filter(&meta, Layer::Semantic, &same));
        let prefix = EntryFilter {
            provenance: Some("seed".into()),
            ..EntryFilter::default()
        };
        assert!(
            !matches_filter(&meta, Layer::Semantic, &prefix),
            "no substring match"
        );
        let absent = EntryMeta::default();
        assert!(!matches_filter(&absent, Layer::Semantic, &same));
    }

    #[test]
    fn promote_candidates_filters_and_sorts() {
        // Qualification: episodic/*.md, entry_valid, confidence>=0.8,
        // settled (mtime>30d or valid_from>30d), not superseded.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        // Settled via a 2020 valid_from (fresh mtime — the backfill clock).
        put(
            &dir,
            "episodic/old-sure.md",
            "---\nconfidence: 0.9\nvalid_from: 2020-01-01T00:00:00Z\n---\nsettled event\n",
        );
        // Boundary confidence 0.8 is inclusive.
        put(
            &dir,
            "episodic/boundary.md",
            "---\nconfidence: 0.8\nvalid_from: 2020-06-01T00:00:00Z\n---\nboundary\n",
        );
        // Young: fresh mtime, no valid_from — still being written.
        put(
            &dir,
            "episodic/young.md",
            "---\nconfidence: 0.95\n---\njust happened\n",
        );
        // Low confidence despite age.
        put(
            &dir,
            "episodic/unsure.md",
            "---\nconfidence: 0.5\nvalid_from: 2020-01-01T00:00:00Z\n---\nshaky\n",
        );
        // Expired (valid_to past) despite age and confidence.
        put(
            &dir,
            "episodic/expired.md",
            "---\nconfidence: 0.95\nvalid_from: 2020-01-01T00:00:00Z\n\
             valid_to: 2000-01-01T00:00:00Z\n---\nstale\n",
        );
        // Superseded despite age and confidence — never re-distilled.
        put(
            &dir,
            "episodic/superseded.md",
            "---\nconfidence: 0.95\nvalid_from: 2020-01-01T00:00:00Z\n---\nold\n\
             superseded_by other.md\n",
        );
        // Wrong layer: an old, confident semantic fact is not a candidate.
        put(
            &dir,
            "semantic/fact.md",
            "---\nconfidence: 0.95\nvalid_from: 2020-01-01T00:00:00Z\n---\nfact\n",
        );
        // Non-topic extension ignored.
        put(&dir, "episodic/notes.txt", "notes\n");

        assert_eq!(
            promote_candidates(&dir),
            vec!["boundary.md".to_string(), "old-sure.md".to_string()]
        );
    }

    #[test]
    fn promote_candidates_caps_at_twenty_sorted() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        for i in 0..25 {
            put(
                &dir,
                &format!("episodic/n{i:02}.md"),
                "---\nconfidence: 0.9\nvalid_from: 2020-01-01T00:00:00Z\n---\nbody\n",
            );
        }
        let got = promote_candidates(&dir);
        assert_eq!(got.len(), 20, "the PROMOTE section is a bounded list");
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(got, sorted, "deterministic ascending order");
        assert!(
            !got.contains(&"n24.md".to_string()),
            "over-cap tail is cut: {got:?}"
        );
    }

    #[test]
    fn consolidate_promote_section_is_add_only() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        put(
            &dir,
            "episodic/old-sure.md",
            "---\nconfidence: 0.9\nvalid_from: 2020-01-01T00:00:00Z\n---\nsettled event\n",
        );
        let before = std::fs::read(dir.join("episodic/old-sure.md")).unwrap();
        let p = FixedProvider::with_reply(
            "---INDEX---\n# Memory Index\n\nold-sure.md — event\n\
             semantic/distilled.md — distilled fact\n---INDEX---",
        );
        consolidate(&p, "tiny", &dir).unwrap();
        let seen = p.seen.lock().unwrap().join("\n");
        assert!(seen.contains("PROMOTE"), "{seen}");
        assert!(seen.contains("old-sure.md"), "{seen}");
        assert!(seen.contains("never move"), "{seen}");
        assert!(seen.contains("ADD"), "{seen}");
        // The episodic body is untouched: promotion adds a semantic/
        // pointer line to the index, never moves/rewrites/deletes.
        let after = std::fs::read(dir.join("episodic/old-sure.md")).unwrap();
        assert_eq!(before, after, "promotion rewrote the episodic body");
    }

    #[test]
    fn consolidate_prompt_marks_empty_promote() {
        let dir = tmpdir();
        ensure(&dir).unwrap();
        let p = FixedProvider::with_reply("---INDEX---\n# Memory Index\n---INDEX---");
        consolidate(&p, "tiny", &dir).unwrap();
        let seen = p.seen.lock().unwrap().join("\n");
        assert!(seen.contains("PROMOTE"), "{seen}");
        assert!(seen.contains("(none"), "{seen}");
    }

    #[test]
    fn fuse_scores_gates_and_denoms() {
        assert_eq!(
            fuse_scores(0.4, 0.9, 0.5, true, true, 0.5),
            0.0,
            "sem under threshold gates"
        );
        assert!((fuse_scores(0.8, 0.0, 0.0, false, false, 0.0) - 0.8).abs() < 1e-6);
        assert!((fuse_scores(0.8, 0.6, 0.0, true, false, 0.0) - 0.7).abs() < 1e-6);
        assert!((fuse_scores(0.8, 0.6, 0.5, true, true, 0.0) - 0.76).abs() < 1e-6);
        assert!((fuse_scores(0.8, 0.0, 0.5, false, true, 0.0) - 1.3 / 1.5).abs() < 1e-6);
        assert_eq!(
            fuse_scores(f32::NAN, 0.5, 0.5, true, true, 0.0),
            0.0,
            "non-finite fails closed"
        );
        assert!(
            fuse_scores(1.0, 1.0, 1.0, true, true, 0.0) <= 1.0,
            "clamped"
        );
    }

    #[test]
    fn normalize_keyword_hits_table_midpoints() {
        for (raw, n) in [(5.0, 1), (7.0, 2), (9.0, 3), (10.0, 4), (12.0, 5)] {
            let got = normalize_keyword(raw, n);
            assert!(
                (got - 0.5).abs() < 1e-9,
                "midpoint saturates at .5: raw={raw} n={n} got={got}"
            );
        }
        assert!(
            (normalize_keyword(5.0, 0) - 0.5).abs() < 1e-9,
            "n_terms clamps to table head"
        );
        assert!(
            (normalize_keyword(12.0, 99) - 0.5).abs() < 1e-9,
            "n_terms clamps to table tail"
        );
        assert!(normalize_keyword(100.0, 1) > 0.99, "saturates high");
        assert!(normalize_keyword(-100.0, 1) < 0.01, "saturates low");
        assert_eq!(normalize_keyword(f64::NAN, 1), 0.0);
    }

    #[test]
    fn entity_link_exact_and_capped_overlap() {
        let boosts = entity_link(&["Paris trip"], &["paris trip"]);
        assert_eq!(boosts, vec![0.5], "exact normalized match pays full boost");
        let partial = entity_link(&["loves paris cafes"], &["paris trip"])[0];
        assert!(
            (partial - 0.25).abs() < 1e-6,
            "one of two entity tokens covered: {partial}"
        );
        assert!(entity_link(&["unrelated text"], &["paris trip"])[0] < 0.25);
        assert_eq!(entity_link(&[""], &["paris"])[0], 0.0);
        assert!(
            entity_link(&["anything"], &["a b c d"])
                .iter()
                .all(|b| *b <= 0.5),
            "cap holds"
        );
    }

    #[test]
    fn content_hash_stable_hex() {
        assert_eq!(
            content_hash(""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(content_hash("abc"), content_hash("abc"));
        assert_ne!(content_hash("abc"), content_hash("abd"));
        assert_eq!(content_hash("abc").len(), 64);
    }

    #[test]
    fn dedupe_by_hash_first_wins_sorted() {
        let entries = vec![
            ("b.md".to_string(), "same".to_string()),
            ("a.md".to_string(), "same".to_string()),
            ("c.md".to_string(), "other".to_string()),
        ];
        let (unique, dupes) = dedupe_by_hash(&entries);
        assert_eq!(
            unique,
            vec!["b.md".to_string(), "c.md".to_string()],
            "first-seen keeps its name"
        );
        assert_eq!(dupes, vec!["a.md".to_string()]);
    }

    #[test]
    fn core_budget_truncates_with_note() {
        let (kept, note) = core_budget("small", 100);
        assert_eq!(kept, "small");
        assert!(note.is_none());
        let (kept, note) = core_budget(&"y".repeat(CORE_CAP + 10), CORE_CAP);
        assert!(
            note.unwrap().contains("exceeds"),
            "repair note, never silent"
        );
        assert!(kept.len() <= CORE_CAP);
        let (kept, note) = core_budget("héllo wörld tail here", 8);
        assert!(note.is_some());
        assert!(kept.len() <= 8, "char-boundary cut, never split mid-char");
        assert!(std::str::from_utf8(kept.as_bytes()).is_ok());
    }

    #[test]
    fn archive_page_sorts_and_continues() {
        let names = vec!["c.md".to_string(), "a.md".to_string(), "b.md".to_string()];
        let (page, next) = archive_page(&names, 0, 2);
        assert_eq!(page, vec!["a.md".to_string(), "b.md".to_string()]);
        assert_eq!(next, Some(2));
        let (page, next) = archive_page(&names, 2, 2);
        assert_eq!(page, vec!["c.md".to_string()]);
        assert_eq!(next, None);
        assert_eq!(archive_page(&names, 9, 2), (Vec::new(), None));
        assert_eq!(archive_page(&names, 0, 0), (Vec::new(), None));
    }

    #[test]
    fn improve_note_renders_drop_add_lines() {
        assert_eq!(improve_note("a\nb", "a\nb"), "", "agreement renders empty");
        let diff = improve_note("a\nb\nc", "b\nc\nd");
        assert!(diff.contains("- drop: a"), "{diff}");
        assert!(diff.contains("+ add: d"), "{diff}");
        let dup = improve_note("x\nx\ny", "x\ny");
        assert_eq!(
            dup.lines().filter(|l| l.starts_with("- drop:")).count(),
            1,
            "{dup}"
        );
    }

    #[test]
    fn tokenize_fts_cjk_phrase_or_sorted() {
        let toks = tokenize_fts("hello 世界");
        assert!(toks.contains(&"hello".to_string()), "{toks:?}");
        assert!(toks.contains(&"世".to_string()), "{toks:?}");
        assert!(toks.contains(&"界".to_string()), "{toks:?}");
        let mut sorted = toks.clone();
        sorted.sort();
        assert_eq!(toks, sorted, "deterministic order");
        assert_eq!(
            tokenize_fts("hi, hi! HI"),
            vec!["hi".to_string()],
            "dedup + lowercase"
        );
        assert!(tokenize_fts("").is_empty());
    }

    #[test]
    fn fts_score_fraction_and_floor() {
        assert_eq!(fts_score(&[], "anything"), 0.0, "no terms matches nothing");
        let half = fts_score(&["hello", "world"], "hello there");
        assert!((half - 0.5).abs() < 1e-6, "{half}");
        assert!(
            (fts_score(&["zzz"], "hello there") - 0.05).abs() < 1e-6,
            "miss is weak evidence, not disproof"
        );
        assert!(
            (fts_score(&["世"], "世界") - 1.0).abs() < 1e-6,
            "CJK single-char matches"
        );
    }

    #[test]
    fn fingerprint_ignores_case_punct() {
        assert_eq!(fingerprint("Hello, World!"), fingerprint("hello world"));
        assert_ne!(fingerprint("hello world"), fingerprint("hello worlds"));
        assert_eq!(fingerprint("abc").len(), 64);
    }

    #[test]
    fn rfc3339_epoch_handles_z_and_offset() {
        let z = rfc3339_epoch("2020-01-01T00:00:00Z").unwrap();
        let off = rfc3339_epoch("2020-01-01T02:00:00+02:00").unwrap();
        assert_eq!(z, off, "the zone offset must shift the epoch");
        assert!(rfc3339_epoch("not-a-date").is_none());
        assert!(rfc3339_epoch("2020-01-02 00:00:00").is_none());
    }
}
