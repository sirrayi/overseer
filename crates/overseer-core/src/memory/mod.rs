//! File-based memory (playbook Ch.3 §9.4; v2 decision record
//! docs/research/2026-09-30-memory-v2.md).
//!
//! A store is a directory of markdown topic files in layer subdirs plus
//! `INDEX.md` (one-line pointers) and `CORE.md` (always-resident lines).
//! v2 runs two stores — user and project (`stores`) — behind the `memory`
//! tool, ranked by BM25F + activation (`index`, `activation`), with
//! recall and prospective reminders (`notice`) and per-session episode
//! notes (`episode`). The legacy `--memory` store lives in the workspace
//! and stays editable with ordinary file tools.
//!
//! Every store is git-versioned (Letta MemFS): free history, diffs,
//! rollback. Commits are engine-made at turn boundaries, not model actions.

pub mod activation;
pub mod amr;
pub mod episode;
pub mod index;
pub mod learn;
mod lock;
pub mod notice;
pub mod pending;
pub mod redact;
pub mod stores;
pub mod threat;

pub use lock::StoreLock;

pub use stores::{overseer_home, Scope};

use std::path::{Path, PathBuf};
use std::process::Command;

pub const INDEX_NAME: &str = "INDEX.md";
/// Resident core file (Letta's `core_memory` pattern, arsenal B2): a small
/// `CORE.md` always in the prompt, next to the index. Where INDEX.md is a
/// pointer table that grows, CORE.md is the fixed handful of lines that
/// must never be paged out — identity and standing instructions.
pub const CORE_NAME: &str = "CORE.md";
/// Hard cap on each resident core block (1,500 bytes ≈ 375 tokens). Over
/// the cap the block is truncated *with a repair note*, never silently.
const CORE_CAP: usize = 1_500;
/// Routing hint (LightRAG pattern, arsenal B2): level-aware retrieval is a
/// prompt contract, not code — tell the model which layer answers which
/// kind of question, and the local/global split falls out of the files.
const ROUTING_HINT: &str = "Retrieval: answer specific questions from \
    topic files (`read` them); answer overview questions from this index.";

/// Ceiling on a declared per-asset TTL (100 years — a bound, not a policy).
const MAX_TTL_DAYS: u64 = 36_500;
/// Playbook cap: the index is a pointer table, not a document store.
const INDEX_CAP: usize = 25_000;

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
    /// v2 retrieval cues (`cues: a, b`), indexed with a 3× boost.
    pub cues: Vec<String>,
    /// v2 prospective trigger (`at:<rfc3339>`, `kw:<a,b>`, `path:<glob>`).
    pub trigger: Option<String>,
    /// v2: when the trigger fired (RFC3339). Set once by the engine.
    pub fired: Option<String>,
    /// v3: where the note came from
    /// (`overseer:session/<id>[#e<event>]`, `amr-import`, …).
    pub source: Option<String>,
    /// v3: the day the note was added, `YYYY-MM-DD`.
    pub added: Option<String>,
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
            cues: Vec::new(),
            trigger: None,
            fired: None,
            source: None,
            added: None,
        }
    }
}

/// Split `text` into `(meta, body)`. No frontmatter → default meta and
/// the whole text as body; malformed frontmatter → Err naming the fault.
/// Range/date/sensitivity validation runs through `validate_meta` so
/// parse and direct construction share one gate.
pub(crate) fn parse_meta(text: &str) -> Result<(EntryMeta, String), String> {
    let mut lines = text.split_inclusive('\n');
    if lines.next().map(str::trim) != Some("---") {
        return Ok((EntryMeta::default(), text.to_string()));
    }
    let (mut off, mut close) = (text.find('\n').map_or(text.len(), |i| i + 1), None);
    let mut header = Vec::new();
    for l in lines {
        off += l.len();
        if l.trim() == "---" {
            close = Some(off);
            break;
        }
        header.push(l);
    }
    let Some(end) = close else {
        return Err("memory: unterminated frontmatter — missing closing `---`".into());
    };
    let mut meta = EntryMeta::default();
    for line in header {
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
            "cues" => {
                meta.cues = v
                    .split(',')
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
                    .collect()
            }
            "trigger" => meta.trigger = Some(v.to_string()).filter(|t| !t.is_empty()),
            "fired" => meta.fired = Some(v.to_string()).filter(|t| !t.is_empty()),
            "source" => meta.source = Some(v.to_string()).filter(|t| !t.is_empty()),
            "added" => meta.added = Some(v.to_string()).filter(|t| !t.is_empty()),
            // Unknown keys are ignored (forward-compatible headers).
            _ => {}
        }
    }
    validate_meta(&meta)?;
    // The body is the text after the closing line, line endings
    // normalized to `\n`; an empty body of a newline-terminated file is
    // one newline.
    let rest = &text[end..];
    let body = if rest.contains('\r') {
        let mut body = rest.lines().collect::<Vec<_>>().join("\n");
        if text.ends_with('\n') {
            body.push('\n');
        }
        body
    } else if rest.is_empty() && text.ends_with('\n') {
        "\n".to_string()
    } else {
        rest.to_string()
    };
    Ok((meta, body))
}

/// The one gate on entry metadata: confidence must be finite and in
/// 0..=1; validity bounds must be RFC3339 when present.
fn validate_meta(meta: &EntryMeta) -> Result<(), String> {
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
        ("fired", &meta.fired),
    ] {
        if let Some(s) = v {
            if !valid_rfc3339(s) {
                return Err(format!(
                    "memory: {name} `{s}` is not RFC3339 (want e.g. 2026-01-02T15:04:05Z)"
                ));
            }
        }
    }
    if let Some(d) = &meta.added {
        if !valid_rfc3339(&format!("{d}T00:00:00Z")) {
            return Err(format!(
                "memory: added `{d}` is not YYYY-MM-DD (want e.g. 2026-01-02)"
            ));
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
    /// Reminders: notes with a `trigger:` that fire once (`notice`).
    Prospective,
}

impl Layer {
    pub const ALL: [Layer; 5] = [
        Layer::Profile,
        Layer::Episodic,
        Layer::Semantic,
        Layer::Procedural,
        Layer::Prospective,
    ];

    /// Subdirectory name under the memory dir.
    pub const fn name(self) -> &'static str {
        match self {
            Layer::Profile => "profile",
            Layer::Episodic => "episodic",
            Layer::Semantic => "semantic",
            Layer::Procedural => "procedural",
            Layer::Prospective => "prospective",
        }
    }

    pub fn parse(name: &str) -> Option<Layer> {
        Layer::ALL.into_iter().find(|l| l.name() == name)
    }

    /// The store a note lands in when no scope is given: identity and
    /// how-tos follow the user, everything else stays with the project.
    pub const fn default_scope(self) -> Scope {
        match self {
            Layer::Profile | Layer::Procedural => Scope::User,
            _ => Scope::Project,
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
            Layer::Prospective => crate::perm::Autonomy::ActAndReport,
            Layer::Semantic => crate::perm::Autonomy::ActSilently,
        }
    }
}

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

/// Memory v2 write bar for a `memory` remember/forget call: the layer is
/// the `layer` input (remember) or the target name's `layer/` segment
/// (forget — `scope:layer/name.md` as search prints it). No recognizable
/// layer fails closed to the approval bar.
pub fn layer_bar_for_input(input: &serde_json::Value) -> crate::perm::Autonomy {
    let field = |k: &str| input.get(k).and_then(serde_json::Value::as_str);
    let layer = if field("op") == Some("forget") {
        field("name")
            .and_then(parse_qualified)
            .map(|(_, layer, _)| layer)
    } else {
        field("layer").and_then(Layer::parse)
    };
    layer.map_or(crate::perm::Autonomy::ActWithApproval, Layer::write_bar)
}

/// A qualified note name — `scope:layer/name.md` or `layer/name.md`, the
/// scope then defaulting by layer as in `remember` — as (scope, layer,
/// rel). The one parser shared by `forget`, distillation SUPERSEDE and the
/// permission gate, so the gate always judges the note the tool writes.
pub fn parse_qualified(name: &str) -> Option<(Scope, Layer, String)> {
    let name = name.trim();
    let (scope, rel) = match name.split_once(':') {
        Some((s, rel)) => (Some(Scope::parse(s)?), rel),
        None => (None, name),
    };
    let (layer, file) = rel.split_once('/')?;
    let layer = Layer::parse(layer)?;
    let stem = file.strip_suffix(".md")?;
    let ok = !stem.is_empty()
        && !stem.starts_with('.')
        && !stem.contains(['/', '\\', ':'])
        && !stem.chars().any(char::is_whitespace);
    ok.then(|| {
        (
            scope.unwrap_or(layer.default_scope()),
            layer,
            rel.to_string(),
        )
    })
}

/// Prompt legend for the memory segment: layer dirs + header keys.
/// Static bytes (prefix-cache safe); the ≤200B cap is asserted in test.
const MEMORY_LEGEND: &str = "Layers: profile/ identity, episodic/ events, \
    semantic/ facts, procedural/ how-to. Headers: provenance, confidence 0-1, \
    sensitivity public|personal|secret.";

/// Create the store owner-only with its layer subdirs, a `.gitignore`
/// keeping the derived `.index/` out of history, and a seeded INDEX.md.
/// Existing files are never overwritten. Returns the index path.
pub fn ensure(dir: &Path) -> std::io::Result<PathBuf> {
    crate::harden::ensure_private_dir(dir)?;
    for layer in Layer::ALL {
        crate::harden::ensure_private_dir(&dir.join(layer.name()))?;
    }
    let ignore = dir.join(".gitignore");
    if !ignore.exists() {
        std::fs::write(&ignore, ".index/\n")?;
    }
    let idx = dir.join(INDEX_NAME);
    if !idx.exists() {
        std::fs::write(&idx, SEED_INDEX)?;
    }
    Ok(idx)
}

/// `valid_to` expiry: the stored instant is strictly before now. Compared
/// as epoch seconds so `Z` and `±HH:MM` stamps order by the instant they
/// name, not by their spelling. An unparseable stamp reads as not expired
/// — `parse_meta` already refuses one, so only a directly constructed
/// `EntryMeta` can carry it.
#[cfg(test)]
fn meta_expired(meta: &EntryMeta) -> bool {
    meta_expired_at(meta, now_secs())
}

fn meta_expired_at(meta: &EntryMeta, now: u64) -> bool {
    meta.valid_to
        .as_deref()
        .and_then(rfc3339_epoch)
        .is_some_and(|to| to < now)
}

/// True when the topic body carries a `superseded_by` trailer pointing at
/// a live successor (F9 — the pointer is stale). Both spellings are
/// accepted: `superseded_by: name` (the frontmatter-ish form) and
/// `superseded_by name` (the ADD-only trailer form). Requiring only the
/// colon was the F9 gap this port closes: a superseded asset stayed
/// "live" to every reader.
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
fn meta_ttl_expired(meta: &EntryMeta, mtime: Option<std::time::SystemTime>, now: u64) -> bool {
    let Some(days) = meta.ttl_days else {
        return false;
    };
    let Some(mtime) = mtime else {
        return false;
    };
    let Ok(modified) = mtime.duration_since(std::time::UNIX_EPOCH) else {
        return false;
    };
    modified
        .as_secs()
        .saturating_add(days.saturating_mul(86_400))
        <= now
}

/// True when an entry's header says it is still current (validity window
/// and TTL not elapsed, not superseded).
fn meta_current(meta: &EntryMeta, text: &str, mtime: Option<std::time::SystemTime>) -> bool {
    meta_current_at(meta, text, mtime, now_secs())
}

/// [`meta_current`] against an explicit clock (epoch seconds).
fn meta_current_at(
    meta: &EntryMeta,
    text: &str,
    mtime: Option<std::time::SystemTime>,
    now: u64,
) -> bool {
    !meta_expired_at(meta, now) && !meta_ttl_expired(meta, mtime, now) && !meta_superseded(text)
}

/// One topic file's parse state — the single scanner every liveness,
/// sensitivity, governance, and view decision reads.
enum Topic {
    /// No backing file (orphan pointer).
    Missing,
    /// File with no frontmatter: documented defaults apply (Personal /
    /// Private) and the whole text is the body.
    Bare(String),
    /// File with a valid header (the meta is boxed — `EntryMeta` grew
    /// `source`/`added` in v3 and dwarfs the other variants).
    Headed(Box<EntryMeta>, String),
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
        Ok((meta, _)) => (Topic::Headed(Box::new(meta), text), mtime),
        Err(_) => (Topic::Malformed, mtime),
    }
}

/// True when a topic body is still current: not expired (`valid_to` or the
/// B2 `ttl_days` clock) and not superseded. This is the entry-level half
/// of the zep validity-interval filter — the INDEX half (`pointer_live`)
/// only drops pointers; bodies are what a fetch actually returns.
fn entry_valid(text: &str, mtime: Option<std::time::SystemTime>) -> bool {
    match parse_meta(text) {
        Ok((meta, _)) => meta_current(&meta, text, mtime),
        // Unparsable header: fail closed (not served as current).
        Err(_) => false,
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

/// True when an INDEX pointer line names a quarantine proposal: unreviewed
/// untrusted text (RT-3). Proposals stay on disk for human review but are
/// never injected into the trusted memory segment.
fn is_proposal_pointer(line: &str) -> bool {
    let mut tokens = line.split_whitespace();
    let first = match tokens.next() {
        Some("-" | "*") => tokens.next(),
        t => t,
    };
    first.is_some_and(|t| t.trim_start_matches('`').starts_with("proposals/"))
}

/// The system-prompt segment carrying the index — sits at the end of the
/// static region (Invariant 2): stable bytes when the index is unchanged,
/// and an edit only invalidates cache from this segment onward. Proposal
/// pointers and expired/superseded pointers are filtered out. Assembled
/// once per session; mid-session edits apply from the next session/resume.
/// An index over the cap is truncated *with a repair note* — never
/// silently. The dir is named relative to the workspace (see
/// `prompt_path`) so the cached prefix carries no machine-specific path.
pub fn index_segment(dir: &Path) -> String {
    index_segment_within(dir, RESIDENT_CAP)
}

/// [`index_segment`] under a total of `cap` bytes: pointer lines are kept
/// in index order while they fit, the rest become `… N more — memory
/// search`.
fn index_segment_within(dir: &Path, cap: usize) -> String {
    let idx = dir.join(INDEX_NAME);
    let text = std::fs::read_to_string(&idx).unwrap_or_default();
    // RT-3: drop proposal pointers (unreviewed) + expired/superseded (F9).
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| !is_proposal_pointer(l) && pointer_live(dir, l))
        .collect();
    let note = if text.len() > INDEX_CAP {
        "\n\n[overseer] INDEX.md exceeds 25KB — prune it: keep only \
         one-line pointers and move detail into topic files."
    } else {
        ""
    };
    let head = format!(
        "## Memory index\n\
         `{}/` is your persistent memory — read and update it with ordinary \
         file tools. {INDEX_NAME} holds one-line pointers (≤25KB); details \
         live in topic files you create there.\n\n{ROUTING_HINT}\n\n",
        prompt_path(dir),
    );
    let tail = format!("{note}\n\n{MEMORY_LEGEND}{}", core_block(dir));
    let budget = cap.saturating_sub(head.len() + tail.len());
    let mut body = String::new();
    let mut shown = 0;
    for (i, line) in lines.iter().enumerate() {
        let sep = usize::from(!body.is_empty());
        if body.len() + sep + line.len() + more_len(lines.len() - i - 1) > budget {
            break;
        }
        if sep == 1 {
            body.push('\n');
        }
        body.push_str(line);
        shown += 1;
    }
    if shown < lines.len() {
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&more_line(lines.len() - shown));
    }
    hard_cap(format!("{head}{body}{tail}"), cap)
}

fn more_line(n: usize) -> String {
    format!("… {n} more — memory search")
}

/// Bytes the `\n… N more` tail takes (0 when nothing is left over).
fn more_len(n: usize) -> usize {
    if n == 0 {
        0
    } else {
        1 + more_line(n).len()
    }
}

/// `text` cut to at most `cap` bytes on a char boundary.
fn hard_cap(mut text: String, cap: usize) -> String {
    if text.len() > cap {
        let mut cut = cap;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
    }
    text
}

/// `dir` as the prompt names it: relative to the workspace (the process
/// cwd) when inside it, else `~`-relative when under `$HOME`, else as-is.
fn prompt_path(dir: &Path) -> String {
    let cwd = std::env::current_dir().ok();
    let home = std::env::var_os("HOME").map(PathBuf::from);
    relative_display(dir, cwd.as_deref(), home.as_deref())
}

fn relative_display(dir: &Path, cwd: Option<&Path>, home: Option<&Path>) -> String {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let under = |base: &Path| -> Option<PathBuf> {
        dir.strip_prefix(base)
            .map(Path::to_path_buf)
            .or_else(|_| canon(dir).strip_prefix(canon(base)).map(Path::to_path_buf))
            .ok()
    };
    if let Some(rel) = cwd.and_then(under) {
        return if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            rel.display().to_string()
        };
    }
    let home = home.filter(|h| h.is_absolute() && h.parent().is_some());
    if let Some(rel) = home.and_then(under) {
        return if rel.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rel.display())
        };
    }
    // DEFERRED(owner): pass the session cwd from prompt::assemble so dirs outside cwd/$HOME render relative — needs prompt.rs
    dir.display().to_string()
}

/// The resident core block (Letta `CORE.md`): the always-in-context handful
/// of lines, capped with a repair note when over budget. Empty when the
/// file is absent — the block is opt-in by existence, like everything else
/// in the memory dir. Parent view only: the quarantined subagent view
/// (`subagent_view`) never carries it — core memory is not scoped per
/// entry, so there is no sensitivity ceiling to apply to it.
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

/// Ordering on sensitivity tiers: Public < Personal < Secret. A filter
/// admits every entry at or below its own tier.
fn admits(filter: Sensitivity, entry: Sensitivity) -> bool {
    rank(entry) <= rank(filter)
}

fn rank(s: Sensitivity) -> u8 {
    match s {
        Sensitivity::Public => 0,
        Sensitivity::Personal => 1,
        Sensitivity::Secret => 2,
    }
}

/// First `*.md` token on an index line, if it is a valid pointer (see
/// `valid_pointer`). Shared with tools/task.rs so the subagent view
/// reads INDEX lines under the same pointer rules.
pub(crate) fn topic_name(line: &str) -> Option<&str> {
    line.split_whitespace()
        .find(|tok| tok.ends_with(".md"))
        .map(trim_pointer)
        .filter(|tok| valid_pointer(tok))
}

fn trim_pointer(tok: &str) -> &str {
    tok.trim_matches(|c| c == '`' || c == '"' || c == '\'' || c == ',' || c == ';')
}

/// A pointer names a topic either bare (`prefs.md`) or qualified by one
/// memory layer (`semantic/prefs.md`). Anything else — other dirs,
/// nesting, `..`, absolute paths, backslashes — is not a pointer.
fn valid_pointer(name: &str) -> bool {
    let file = match name.split_once('/') {
        None => name,
        Some((layer, file)) if Layer::ALL.iter().any(|l| l.name() == layer) => file,
        Some(_) => return false,
    };
    file.len() > ".md".len() && file.ends_with(".md") && !file.contains('/') && !file.contains('\\')
}

/// The memory-relative path (`/`-separated) a pointer resolves to: a
/// qualified pointer names its exact layer file; a bare one is looked up
/// in the memory root first, then each layer subdir. None when the
/// pointer is invalid or no backing file exists.
fn resolve_pointer(dir: &Path, name: &str) -> Option<String> {
    resolve_pointer_with(name, |rel| dir.join(rel).is_file())
}

fn resolve_pointer_with(name: &str, is_file: impl Fn(&str) -> bool) -> Option<String> {
    if !valid_pointer(name) {
        return None;
    }
    if is_file(name) {
        return Some(name.to_string());
    }
    if name.contains('/') {
        return None;
    }
    Layer::ALL
        .iter()
        .map(|l| format!("{}/{name}", l.name()))
        .find(|rel| is_file(rel))
}

/// Identity of a pointer for "same topic?" comparisons: its resolved
/// relative path, or the name as written when nothing backs it — so
/// `foo.md` and `semantic/foo.md` agree when both resolve to one file.
fn pointer_key(dir: &Path, name: &str) -> String {
    resolve_pointer(dir, name).unwrap_or_else(|| name.to_string())
}

/// True when some line of `index` names the topic keyed `key` (see
/// `pointer_key`) as a whole token.
fn index_names(dir: &Path, index: &str, key: &str) -> bool {
    index.lines().any(|l| {
        l.split_whitespace()
            .map(trim_pointer)
            .any(|tok| tok.ends_with(".md") && valid_pointer(tok) && pointer_key(dir, tok) == key)
    })
}

/// Locate a topic file by pointer name (bare or layer-qualified, see
/// `resolve_pointer`). None when no backing file exists.
pub fn layer_path(dir: &Path, name: &str) -> Option<PathBuf> {
    resolve_pointer(dir, name).map(|rel| dir.join(rel))
}

/// Git-version the memory dir. Runs `git init` once, then commits any dirty
/// state. Best-effort: memory works without history, so failures are
/// swallowed (no git binary, read-only fs) rather than killing the turn.
/// Every commit first regenerates the store's AMR `MEMORY.md` (§9.1) so
/// the entry point and the history stay in sync.
pub fn commit(dir: &Path, msg: &str) {
    // AMR entry point (§9.1). Not behind git: a store without history
    // still gets its MEMORY.md. Best-effort like the commit itself.
    let _ = amr::regen(dir, now_secs());
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

/// `overseer memory log`: the store's git history, `--oneline`, newest
/// first, capped. Empty when the store has no history or git is absent.
pub fn git_log(dir: &Path, cap: usize) -> Vec<String> {
    if !dir.join(".git").exists() {
        return Vec::new();
    }
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["log", "--oneline", &format!("-{cap}")])
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| l.to_string())
        .collect()
}

/// `overseer memory restore` (§8): un-expire a forgotten note — clear
/// the `valid_to` frontmatter and the `forgotten:` body line `expire_note`
/// wrote. The INDEX pointer lives (forget never deletes it). Takes the
/// store lock and commits, like every write path.
pub fn restore(dir: &Path, rel: &str) -> Result<String, String> {
    if rel.contains("..") || rel.starts_with('/') || rel.starts_with("pending/") {
        return Err(format!("memory: bad note path `{rel}`"));
    }
    let path = dir.join(rel);
    let _lock = StoreLock::acquire(dir).map_err(|e| e.to_string())?;
    let text = std::fs::read_to_string(&path).map_err(|e| format!("memory: read `{rel}`: {e}"))?;
    if !text
        .lines()
        .any(|l| l.trim_start().starts_with("valid_to:"))
        && !text
            .lines()
            .any(|l| l.trim_start().starts_with("forgotten:"))
    {
        return Err(format!("memory: `{rel}` is not expired"));
    }
    let text = clear_forgotten(&clear_meta_key(&text, "valid_to"));
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    commit(dir, &format!("memory: restore {rel}"));
    Ok(format!("restored {rel}"))
}

/// A reconciliation plan over the memory dir (mem0 pattern, arsenal B2):
/// what the index claims versus what the directory actually holds, decided
/// before any model call. Three verdicts, no model involved:
///
/// - `drop` — a pointer whose asset is missing, expired, or superseded;
/// - `add`  — a topic file that no pointer names;
/// - `keep` — a pointer and its asset agree (counted, not listed).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ReconcilePlan {
    drop: Vec<String>,
    add: Vec<String>,
    keep: usize,
}

impl ReconcilePlan {
    /// Render the plan for the consolidation prompt — the model sees
    /// exactly what the engine already decided.
    fn render(&self) -> String {
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
fn reconcile(dir: &Path, index_text: &str) -> ReconcilePlan {
    // (name as written, resolved key) — one entry per distinct topic.
    let mut named: Vec<(String, String)> = Vec::new();
    for line in index_text.lines() {
        // Proposals are never part of the trusted index (RT-3).
        if is_proposal_pointer(line) {
            continue;
        }
        if let Some(n) = topic_name(line) {
            let key = pointer_key(dir, n);
            if !named.iter().any(|(_, k)| *k == key) {
                named.push((n.to_string(), key));
            }
        }
    }
    named.sort();

    let mut plan = ReconcilePlan::default();
    for (n, _) in &named {
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

    // Untracked topics: a file on disk no pointer resolves to. Root and
    // the layer subdirs both count.
    let mut dirs = vec![(dir.to_path_buf(), None)];
    for layer in Layer::ALL {
        dirs.push((dir.join(layer.name()), Some(layer.name())));
    }
    for (d, layer) in dirs {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if !name.ends_with(".md") || name == INDEX_NAME || name == CORE_NAME {
                continue;
            }
            let rel = match layer {
                Some(l) => format!("{l}/{name}"),
                None => name.clone(),
            };
            if !named.iter().any(|(_, k)| *k == rel) {
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
/// file stays on disk untouched (bodies are append-only — superseded via
/// a `superseded_by` trailer). Qualification (every clause must hold):
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
fn promote_candidates(dir: &Path) -> Vec<String> {
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
/// as settled). Zero-dep days-from-civil (Howard Hinnant's algorithm),
/// with the numeric zone offset
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
        cache_key: None,
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
    // The cap bounds the model's reply only; the restore pass below may
    // push the written INDEX past INDEX_CAP. Intended: a live pointer's
    // survival outranks the cap (index_segment still truncates what the
    // prompt carries, with a repair note).
    let mut capped: String = new_index.chars().take(INDEX_CAP).collect();
    // mem0 reconcile enforcement: pointers the plan marked stale never
    // come back, whatever the model replied (ADD-only: this can only drop
    // a pointer whose backing file is gone or whose asset expired).
    let drop_keys: Vec<String> = plan.drop.iter().map(|d| pointer_key(dir, d)).collect();
    let mut dropped_stale = 0usize;
    if !plan.drop.is_empty() {
        let kept: Vec<&str> = capped
            .lines()
            .filter(|l| {
                let stale = topic_name(l).is_some_and(|n| drop_keys.contains(&pointer_key(dir, n)));
                if stale {
                    dropped_stale += 1;
                }
                !stale
            })
            .collect();
        capped = kept.join("\n");
    }
    // ADD-only enforcement: every live pointer in the old index survives.
    // A pointer the reply no longer names is re-appended with its original
    // line; one the reply rewrote, merged, or re-qualified (still naming
    // the same resolved topic, e.g. behind a `superseded_by` trailer or
    // promoted from `foo.md` to `semantic/foo.md`) counts as kept.
    let mut restored = 0usize;
    for line in old_index.lines() {
        if is_proposal_pointer(line) {
            continue;
        }
        let Some(name) = topic_name(line) else {
            continue;
        };
        let key = pointer_key(dir, name);
        if drop_keys.contains(&key) || index_names(dir, &capped, &key) {
            continue;
        }
        if !capped.is_empty() && !capped.ends_with('\n') {
            capped.push('\n');
        }
        capped.push_str(line);
        restored += 1;
    }
    std::fs::write(&idx, format!("{capped}\n")).map_err(|e| e.to_string())?;
    commit(dir, "consolidate");

    let dropped = old_index
        .lines()
        .filter(|l| l.contains(".md"))
        .filter(|l| !capped.contains(l.trim()))
        .count();
    Ok(format!(
        "consolidated: {} → {} index lines, {dropped} pointers dropped \
         (reconcile: {} stale, {} untracked, {dropped_stale} stale reclaimed, \
         {restored} live restored)",
        old_index.lines().count(),
        capped.lines().count(),
        plan.drop.len(),
        plan.add.len()
    ))
}

/// Wall clock as Unix epoch seconds (0 before the epoch).
pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `secs` since the epoch as an RFC3339 UTC stamp (`…T…Z`). Inverse of
/// `rfc3339_epoch` (civil-from-days, Howard Hinnant's algorithm).
pub(crate) fn rfc3339(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let sod = secs % 86_400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        sod / 3600,
        (sod % 3600) / 60,
        sod % 60
    )
}

/// A note's title: its first `# ` heading, else its first non-empty line.
pub fn title_of(body: &str) -> &str {
    body.lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("# "))
        .or_else(|| body.lines().map(str::trim).find(|l| !l.is_empty()))
        .map(str::trim)
        .unwrap_or("")
}

/// `[[name]]` link targets in a note body, in order of appearance.
pub fn links_of(body: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(i) = rest.find("[[") {
        rest = &rest[i + 2..];
        let Some(j) = rest.find("]]") else { break };
        let target = rest[..j].trim();
        if !target.is_empty() && !target.contains('\n') {
            out.push(target);
        }
        rest = &rest[j + 2..];
    }
    out
}

/// `text` with frontmatter key `key` set to `value`: an existing line is
/// replaced, else the line is added before the closing `---` (a file with
/// no frontmatter gains one). The body is never touched.
pub(crate) fn set_meta_key(text: &str, key: &str, value: &str) -> String {
    let line = format!("{key}: {value}");
    let mut lines: Vec<&str> = text.lines().collect();
    let close = (lines.first().map(|l| l.trim()) == Some("---"))
        .then(|| lines.iter().skip(1).position(|l| l.trim() == "---"))
        .flatten()
        .map(|i| i + 1);
    let Some(close) = close else {
        return format!("---\n{line}\n---\n{text}");
    };
    let prefix = format!("{key}:");
    match lines[1..close]
        .iter()
        .position(|l| l.trim_start().starts_with(&prefix))
    {
        Some(i) => lines[i + 1] = &line,
        None => lines.insert(close, &line),
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `text` with the frontmatter `key` line removed (no-op when absent).
/// The body is never touched; a `forgotten:` line is a *body* line, so
/// [`clear_forgotten`] handles that one separately.
pub(crate) fn clear_meta_key(text: &str, key: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    let close = (lines.first().map(|l| l.trim()) == Some("---"))
        .then(|| lines.iter().skip(1).position(|l| l.trim() == "---"))
        .flatten()
        .map(|i| i + 1);
    let Some(close) = close else {
        return text.to_string();
    };
    let prefix = format!("{key}:");
    if let Some(i) = lines[1..close]
        .iter()
        .position(|l| l.trim_start().starts_with(&prefix))
    {
        lines.remove(i + 1);
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `text` without the `forgotten: <reason>` body line a `forget` wrote.
pub(crate) fn clear_forgotten(text: &str) -> String {
    let mut lines: Vec<&str> = text.lines().collect();
    lines.retain(|l| !l.trim_start().starts_with("forgotten:"));
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// `text` expired at `now` (`valid_to` set) with a trailing
/// `forgotten: <reason>` body line — the one expiry shape shared by the
/// `forget` op, review apply and pending-op approval.
pub(crate) fn expire_note(text: &str, reason: &str, now: u64) -> String {
    let mut text = set_meta_key(text, "valid_to", &rfc3339(now));
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let reason = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    text.push_str(&format!("forgotten: {reason}\n"));
    text
}

/// Rewrite the INDEX.md pointer line that names `rel` so its title reads
/// `title`. The pointer table is ADD-only for the *model*; the engine
/// keeping a superseded note's pointer honest is housekeeping, not a
/// model write. No-op when no pointer names `rel`.
pub(crate) fn update_pointer(dir: &Path, rel: &str, title: &str) -> std::io::Result<()> {
    let path = dir.join(INDEX_NAME);
    let old = std::fs::read_to_string(&path)?;
    let mut changed = false;
    let out: Vec<String> = old
        .lines()
        .map(|line| {
            let hit = topic_name(line)
                .and_then(|n| resolve_pointer(dir, n))
                .is_some_and(|r| r == rel);
            if hit {
                changed = true;
                format!("{rel} — {title}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if changed {
        std::fs::write(
            &path,
            format!("{}\n", out.join("\n").trim_end_matches('\n')),
        )?;
    }
    Ok(())
}

/// Append one pointer line to a store's INDEX.md (ADD-only).
pub(crate) fn append_pointer(dir: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write;
    let idx = dir.join(INDEX_NAME);
    let old = std::fs::read_to_string(&idx).unwrap_or_default();
    let sep = if old.is_empty() || old.ends_with('\n') {
        ""
    } else {
        "\n"
    };
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&idx)?;
    f.write_all(format!("{sep}{line}\n").as_bytes())
}

/// Pointer titles are capped so one INDEX line stays one short line.
pub(crate) const TITLE_MAX: usize = 80;

/// A note's pointer title: [`title_of`], whitespace-collapsed, capped.
pub(crate) fn pointer_title(text: &str) -> String {
    title_of(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(TITLE_MAX)
        .collect()
}

/// `base`, else the first free `base-2`, `base-3`, … among `dir/*.md`.
/// Lowercase hex of `sha256(bytes)`, first `n` chars.
pub(crate) fn sha_hex(bytes: &[u8], n: usize) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
        .chars()
        .take(n)
        .collect()
}

/// Create `dir/<base>.md` (else `<base>-2.md`, `-3`, …) holding `text`,
/// claiming the name with `create_new` so a concurrent writer can never
/// be overwritten. Returns the name without `.md`.
pub(crate) fn create_unique(dir: &Path, base: &str, text: &str) -> std::io::Result<String> {
    use std::io::Write;
    let mut n = 1;
    loop {
        let name = if n == 1 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dir.join(format!("{name}.md")))
        {
            Ok(mut f) => {
                f.write_all(text.as_bytes())?;
                return Ok(name);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => n += 1,
            Err(e) => return Err(e),
        }
    }
}

/// ADD a new note `layer/<unique base>.md` to store `dir` with frontmatter
/// lines `meta` (each `\n`-terminated), then append its one pointer line.
/// Returns the store-relative path.
pub(crate) fn add_note(
    dir: &Path,
    layer: Layer,
    base: &str,
    meta: &str,
    text: &str,
) -> std::io::Result<String> {
    let ldir = dir.join(layer.name());
    crate::harden::ensure_private_dir(&ldir)?;
    let name = create_unique(&ldir, base, &format!("---\n{meta}---\n{text}\n"))?;
    let rel = format!("{}/{name}.md", layer.name());
    append_pointer(dir, &format!("{rel} — {}", pointer_title(text)))?;
    Ok(rel)
}

/// Consolidate v2 over the resolved stores: per store, v1's index pass
/// then the use-journal compaction; then one distillation of the project
/// store's new episodes ([`episode::distill`]); then a commit per store.
pub fn consolidate_stores(
    provider: &dyn crate::provider::Provider,
    model: &str,
    stores: &[(Scope, PathBuf)],
    now: u64,
) -> Result<String, String> {
    let mut out = Vec::new();
    for (scope, dir) in stores {
        let msg = consolidate(provider, model, dir)?;
        let n = activation::compact(dir).map_err(|e| e.to_string())?;
        out.push(format!(
            "{}: {msg}; uses compacted for {n} notes",
            scope.name()
        ));
    }
    let d = episode::distill(provider, model, stores, now)?;
    for (_, dir) in stores {
        commit(dir, "consolidate: distill");
    }
    out.push(format!(
        "distilled {} episodes: {} added, {} superseded, {} duplicate, {} invalid lines skipped",
        d.episodes,
        d.added.len(),
        d.superseded,
        d.duplicates,
        d.invalid
    ));
    Ok(out.join("\n"))
}

/// Hard cap on the resident memory segment, bytes (≈1K tokens), legacy
/// `--memory` text included.
pub const RESIDENT_CAP: usize = 4_000;
/// At most this many ranked index lines are resident.
pub const RESIDENT_LINES: usize = 24;

const CONTRACT: &str = "Use the memory tool: search before asking what you may \
    already know; remember durable facts, preferences and outcomes.";

const EMPTY_RESIDENT: &str = "## Memory\nNo notes yet. Use the memory tool to \
    remember durable facts, preferences and outcomes across sessions.";

/// The v2 resident segment for `config`, or None when memory is off.
/// Assembled once per Agent (prompt.rs); see [`resident`].
pub fn resident_segment(config: &crate::agent::AgentConfig) -> Option<String> {
    let stores = stores::of_config(config);
    if stores.is_empty() {
        return None;
    }
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let legacy = config
        .memory_dir
        .as_deref()
        .filter(|d| canon(d).starts_with(canon(&config.cwd)));
    let Some(dir) = legacy else {
        return Some(resident(&stores, now_secs()));
    };
    let user: Vec<_> = stores
        .into_iter()
        .filter(|(s, _)| *s == Scope::User)
        .collect();
    let user = resident_within(&user, now_secs(), RESIDENT_CAP / 2);
    let legacy = index_segment_within(dir, RESIDENT_CAP - user.len() - 2);
    Some(format!("{legacy}\n\n{user}"))
}

/// The resident memory text over `stores` at clock `now`: the contract,
/// each store's CORE.md (≤[`CORE_CAP`], repair note beyond), then at most
/// [`RESIDENT_LINES`] live INDEX pointers as `scope:layer/name.md — …`,
/// ranked by activation (desc, ties by name). Hard-capped at
/// [`RESIDENT_CAP`] bytes with a `… N more` tail. Deterministic: same
/// stores + same `now` → same bytes. No absolute paths.
pub fn resident(stores: &[(Scope, PathBuf)], now: u64) -> String {
    resident_within(stores, now, RESIDENT_CAP)
}

fn resident_within(stores: &[(Scope, PathBuf)], now: u64, cap: usize) -> String {
    let head = format!("## Memory\n{CONTRACT}");
    let mut cores = String::new();
    for (scope, dir) in stores {
        let core = core_block(dir);
        if !core.is_empty() {
            cores.push_str(&core.replacen(
                "## Memory core",
                &format!("## {} core", scope.name()),
                1,
            ));
        }
    }
    let mut ranked: Vec<(f64, String)> = Vec::new();
    for (scope, dir) in stores {
        let uses = activation::load(dir);
        for (rel, rest) in live_pointers(dir, now) {
            let mtime = std::fs::metadata(dir.join(&rel))
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_secs());
            let b = activation::base_level(
                &uses
                    .get(&rel)
                    .cloned()
                    .unwrap_or_else(|| activation::Uses::once(mtime)),
                now,
            );
            let line = if rest.is_empty() {
                format!("{}:{rel}", scope.name())
            } else {
                format!("{}:{rel} {rest}", scope.name())
            };
            ranked.push((b, line));
        }
    }
    if ranked.is_empty() && cores.is_empty() {
        return EMPTY_RESIDENT.to_string();
    }
    ranked.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let mut out = format!("{head}{cores}");
    if !ranked.is_empty() {
        out.push_str("\n\n## Memory index");
    }
    let total = ranked.len();
    let mut shown = 0;
    for (i, (_, line)) in ranked.iter().take(RESIDENT_LINES).enumerate() {
        if out.len() + 1 + line.len() + more_len(total - i - 1) > cap {
            break;
        }
        out.push('\n');
        out.push_str(line);
        shown += 1;
    }
    if shown < total {
        out.push('\n');
        out.push_str(&more_line(total - shown));
    }
    hard_cap(out, cap)
}

/// INDEX pointers of one store as `(resolved rel path, rest of the line)`:
/// proposals and orphans dropped, first line per topic wins, index order
/// kept. Liveness is the caller's call.
pub(crate) fn pointer_lines(dir: &Path) -> Vec<(String, String)> {
    let index = std::fs::read_to_string(dir.join(INDEX_NAME)).unwrap_or_default();
    pointer_lines_in(&index, |rel| dir.join(rel).is_file())
}

/// [`pointer_lines`] over INDEX text `index`, with `is_topic(rel)` saying
/// which store-relative topic files exist.
pub(crate) fn pointer_lines_in(
    index: &str,
    is_topic: impl Fn(&str) -> bool,
) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for line in index.lines() {
        if is_proposal_pointer(line) {
            continue;
        }
        let Some(name) = topic_name(line) else {
            continue;
        };
        let Some(rel) = resolve_pointer_with(name, &is_topic) else {
            continue;
        };
        if !seen.insert(rel.clone()) {
            continue;
        }
        let at = line.find(name).map_or(line.len(), |i| i + name.len());
        let rest = line[at..]
            .trim_start_matches(['`', '"', '\'', ',', ';'])
            .trim()
            .to_string();
        out.push((rel, rest));
    }
    out
}

/// [`pointer_lines`] whose topic is current at `now` (bare files count;
/// malformed headers fail closed).
pub(crate) fn live_pointers(dir: &Path, now: u64) -> Vec<(String, String)> {
    pointer_lines(dir)
        .into_iter()
        .filter(|(rel, _)| match topic_of(dir, rel) {
            (Topic::Bare(_), _) => true,
            (Topic::Headed(meta, text), mtime) => meta_current_at(&meta, &text, mtime, now),
            (Topic::Missing | Topic::Malformed, _) => false,
        })
        .collect()
}

/// The instant a current entry stops being current (earliest of
/// `valid_to` and the TTL clock), for index invalidation.
pub(crate) fn expires_at(meta: &EntryMeta, mtime: u64) -> Option<u64> {
    // `valid_to` is inclusive (expired once strictly past), the TTL is not.
    let to = meta
        .valid_to
        .as_deref()
        .and_then(rfc3339_epoch)
        .map(|t| t.saturating_add(1));
    let ttl = meta
        .ttl_days
        .map(|d| mtime.saturating_add(d.saturating_mul(86_400)));
    to.into_iter().chain(ttl).min()
}

/// v1 liveness of a parsed entry at clock `now`.
pub(crate) fn current_at(meta: &EntryMeta, text: &str, mtime: u64, now: u64) -> bool {
    let mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(mtime);
    meta_current_at(meta, text, Some(mtime), now)
}

/// letta P1: the `core_block` cap as a reusable pure fn. Under budget
/// the (trimmed) text rides with no note; over budget the head is cut
/// on a char boundary (a multibyte char is never split) and rides with
/// a repair note — never a silent cut.
fn core_budget(text: &str, cap: usize) -> (String, Option<String>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The slicing parse keeps the line-joining parse's body exactly.
    #[test]
    fn parse_meta_body_matches_the_line_join() {
        fn joined(text: &str) -> String {
            let all: Vec<&str> = text.lines().collect();
            let end = (1..all.len()).find(|&i| all[i].trim() == "---").unwrap();
            let mut body = all[end + 1..].join("\n");
            if text.ends_with('\n') {
                body.push('\n');
            }
            body
        }
        for text in [
            "---\nconfidence: 0.5\n---\n",
            "---\nconfidence: 0.5\n---",
            "---\n---\nbody\n",
            "---\n---\nbody",
            "---\ncues: a, b\n---\n# T\n\nx\n\n",
            "---\r\nconfidence: 0.5\r\n---\r\nline\r\nmore\r\n",
            "---\n---\r\nmixed\nends\r\n",
            " --- \n---\n\n\n",
        ] {
            let (_, body) = parse_meta(text).unwrap();
            assert_eq!(body, joined(text), "{text:?}");
        }
    }

    fn format_utc_stamp(secs: u64) -> String {
        rfc3339(secs)
    }

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
        assert!(seg.len() <= RESIDENT_CAP, "{}", seg.len());
    }

    #[test]
    fn segment_names_the_dir_without_absolute_machine_paths() {
        let cwd = std::env::current_dir().unwrap();
        let dir = cwd.join(format!(".overseer-mem-test-{}", uuid::Uuid::now_v7()));
        ensure(&dir).unwrap();
        let seg = index_segment(&dir);
        let _ = std::fs::remove_dir_all(&dir);
        let rel = dir.file_name().unwrap().to_string_lossy().to_string();
        assert!(seg.contains(&format!("`{rel}/`")), "{seg}");
        assert!(!seg.contains(&cwd.display().to_string()), "{seg}");
        if let Some(home) = std::env::var_os("HOME").filter(|h| h.len() > 1) {
            let home = PathBuf::from(home).display().to_string();
            assert!(!seg.contains(&home), "absolute home path leaked: {seg}");
        }
    }

    #[test]
    fn relative_display_prefers_workspace_then_home() {
        let cwd = Path::new("/w/proj");
        let home = Path::new("/h/me");
        let show = |d: &str| relative_display(Path::new(d), Some(cwd), Some(home));
        assert_eq!(show("/w/proj/memory"), "memory");
        assert_eq!(show("/w/proj"), ".");
        assert_eq!(show("/h/me/.overseer/memory"), "~/.overseer/memory");
        assert_eq!(show("/srv/mem"), "/srv/mem");
        assert_eq!(
            relative_display(Path::new("/x"), None, Some(Path::new("/"))),
            "/x",
            "a root HOME is no prefix"
        );
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
    fn consolidate_restores_live_pointers_the_model_omitted() {
        // ADD-only: the model may add and merge, never drop a live pointer.
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(
            &idx,
            "# Memory Index\n\nfacts.md — user facts\nprefs.md — `tabs` over spaces\ngone.md — missing\n",
        )
        .unwrap();
        std::fs::write(dir.join("facts.md"), "data").unwrap();
        std::fs::write(dir.join("semantic/prefs.md"), "tabs").unwrap();
        let p = FixedProvider::with_reply(
            "---INDEX---\n# Memory Index\n\nfacts.md — facts, tightened\nnew.md — added\n---INDEX---",
        );
        let msg = consolidate(&p, "tiny", &dir).unwrap();
        let new = std::fs::read_to_string(&idx).unwrap();
        assert!(
            new.contains("prefs.md — `tabs` over spaces"),
            "omitted live pointer restored verbatim: {new}"
        );
        assert!(new.contains("facts.md — facts, tightened"), "{new}");
        assert!(
            !new.contains("user facts"),
            "a rewritten pointer is not duplicated: {new}"
        );
        assert!(new.contains("new.md — added"), "additions stay: {new}");
        assert!(!new.contains("gone.md"), "stale pointer still drops: {new}");
        assert!(msg.contains("1 live restored"), "{msg}");
    }

    #[test]
    fn consolidate_restores_an_omitted_layer_qualified_pointer() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(
            &idx,
            "# Memory Index\n\nfacts.md — facts\nsemantic/prefs.md — `tabs` over spaces\n",
        )
        .unwrap();
        std::fs::write(dir.join("facts.md"), "data").unwrap();
        std::fs::write(dir.join("semantic/prefs.md"), "tabs").unwrap();
        let p = FixedProvider::with_reply(
            "---INDEX---\n# Memory Index\n\nfacts.md — facts\n---INDEX---",
        );
        let msg = consolidate(&p, "tiny", &dir).unwrap();
        let new = std::fs::read_to_string(&idx).unwrap();
        assert!(
            new.lines()
                .any(|l| l == "semantic/prefs.md — `tabs` over spaces"),
            "omitted qualified pointer restored verbatim: {new}"
        );
        assert!(msg.contains("1 live restored"), "{msg}");
    }

    #[test]
    fn consolidate_bare_to_qualified_promotion_is_not_duplicated() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(&idx, "# Memory Index\n\nfoo.md — foo\n").unwrap();
        std::fs::write(dir.join("semantic/foo.md"), "foo body").unwrap();
        let p = FixedProvider::with_reply(
            "---INDEX---\n# Memory Index\n\nsemantic/foo.md — foo, promoted\n---INDEX---",
        );
        let msg = consolidate(&p, "tiny", &dir).unwrap();
        let new = std::fs::read_to_string(&idx).unwrap();
        assert_eq!(
            new.lines().filter(|l| l.contains("foo.md")).count(),
            1,
            "a promoted pointer still names the topic: {new}"
        );
        assert!(new.contains("semantic/foo.md — foo, promoted"), "{new}");
        assert!(msg.contains("0 live restored"), "{msg}");
    }

    #[test]
    fn qualified_pointer_to_a_missing_file_is_dropped() {
        let dir = tmpdir();
        let idx = ensure(&dir).unwrap();
        std::fs::write(dir.join("semantic/live.md"), "body\n").unwrap();
        let index = "# Memory Index\n\nsemantic/live.md — fine\nsemantic/gone.md — missing\n";
        std::fs::write(&idx, index).unwrap();

        let plan = reconcile(&dir, index);
        assert_eq!(plan.drop, vec!["semantic/gone.md".to_string()]);
        assert_eq!(plan.keep, 1, "semantic/live.md agrees");
        assert!(
            plan.add.is_empty(),
            "a qualified pointer tracks its file: {plan:?}"
        );

        // The model lazily keeps the stale qualified pointer: the engine
        // reclaims it and never restores it.
        let p = FixedProvider::with_reply(
            "---INDEX---\n# Memory Index\n\nsemantic/gone.md — missing\n---INDEX---",
        );
        let msg = consolidate(&p, "tiny", &dir).unwrap();
        let new = std::fs::read_to_string(&idx).unwrap();
        assert!(!new.contains("semantic/gone.md"), "{new}");
        assert!(new.contains("semantic/live.md — fine"), "{new}");
        assert!(msg.contains("1 stale reclaimed"), "{msg}");
    }

    #[test]
    fn topic_name_accepts_bare_and_layer_qualified_pointers_only() {
        assert_eq!(topic_name("prefs.md — tabs"), Some("prefs.md"));
        for layer in Layer::ALL {
            let line = format!("{}/x.md — y", layer.name());
            let want = format!("{}/x.md", layer.name());
            assert_eq!(topic_name(&line), Some(want.as_str()));
        }
        for bad in [
            "other/x.md — y",
            "semantic/../x.md — y",
            "semantic/a/x.md — y",
            "/abs/x.md — y",
            "semantic/.md — y",
            "semantic\\x.md — y",
            "proposals/x.md — y",
        ] {
            assert_eq!(topic_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn qualified_names_parse_with_layer_default_scopes() {
        let q = |n: &str| parse_qualified(n).map(|(s, l, r)| (s.name(), l.name(), r));
        assert_eq!(
            q("semantic/db.md"),
            Some(("project", "semantic", "semantic/db.md".into()))
        );
        assert_eq!(
            q("procedural/fmt.md"),
            Some(("user", "procedural", "procedural/fmt.md".into()))
        );
        assert_eq!(
            q("user:semantic/db.md"),
            Some(("user", "semantic", "semantic/db.md".into()))
        );
        for bad in [
            "db",
            "db.md",
            "semantic/db",
            "semantic/.md",
            "semantic/a/db.md",
            "other/db.md",
            "team:semantic/db.md",
            "semantic/my note.md",
            "proposals/db.md",
        ] {
            assert_eq!(q(bad), None, "{bad}");
        }
    }

    #[test]
    fn create_unique_never_overwrites_under_a_race() {
        let dir = tmpdir();
        std::fs::write(dir.join("n.md"), "first\n").unwrap();
        let names: Vec<String> = std::thread::scope(|s| {
            let hs: Vec<_> = (0..16)
                .map(|i| {
                    let dir = &dir;
                    s.spawn(move || create_unique(dir, "n", &format!("body {i}\n")).unwrap())
                })
                .collect();
            hs.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let mut sorted = names.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), 16, "{names:?}");
        assert!(!names.contains(&"n".to_string()));
        assert_eq!(
            std::fs::read_to_string(dir.join("n.md")).unwrap(),
            "first\n"
        );
        for (i, name) in names.iter().enumerate() {
            let body = std::fs::read_to_string(dir.join(format!("{name}.md"))).unwrap();
            assert_eq!(body, format!("body {i}\n"));
        }
    }

    #[test]
    fn proposal_pointer_is_decided_by_the_first_token() {
        for yes in [
            "proposals/x.md — y",
            "  proposals/x.md — y",
            "- proposals/x.md — y",
            "`proposals/x.md` — y",
        ] {
            assert!(is_proposal_pointer(yes), "{yes}");
        }
        for no in [
            "semantic/review.md — how proposals/ are reviewed",
            "semantic/x.md — see docs/proposals/rfc.md",
            "",
        ] {
            assert!(!is_proposal_pointer(no), "{no}");
        }
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
        assert!(seg.len() <= RESIDENT_CAP, "{}", seg.len());

        // CORE.md is not a topic: it must never be reconciled as one.
        std::fs::write(dir.join(CORE_NAME), "core\n").unwrap();
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
    fn write_bar_gates_identity_above_world_facts() {
        use crate::perm::Autonomy;
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
        assert!(matches!(
            subagent_view(&dir, "live.md", Sensitivity::Secret),
            View::Admitted(_)
        ));
        assert!(
            matches!(
                subagent_view(&dir, "gone.md", Sensitivity::Secret),
                View::Hidden
            ),
            "ttl_days=0 has expired"
        );

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
    }

    #[test]
    fn space_form_superseded_trailer_retires_the_asset() {
        // F9: the reader accepts `superseded_by name` (no colon), so the
        // pointer drops and the body stops being served.
        let dir = tmpdir();
        ensure(&dir).unwrap();
        let p = dir.join("episodic").join("old.md");
        std::fs::write(&p, "old content\nsuperseded_by new.md\n").unwrap();
        assert!(
            matches!(
                subagent_view(&dir, "old.md", Sensitivity::Secret),
                View::Hidden
            ),
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

    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// RFC3339 stamp for instant `epoch` written in the zone `off_mins`
    /// east of UTC (0 → `Z`).
    fn stamp_at(epoch: u64, off_mins: i64) -> String {
        let wall = format_utc_stamp((epoch as i64 + off_mins * 60) as u64);
        if off_mins == 0 {
            return wall;
        }
        let sign = if off_mins < 0 { '-' } else { '+' };
        let a = off_mins.abs();
        format!("{}{sign}{:02}:{:02}", &wall[..19], a / 60, a % 60)
    }

    #[test]
    fn valid_to_expiry_compares_instants_across_offsets() {
        let now = now_secs();
        let meta = |to: String| EntryMeta {
            valid_to: Some(to),
            ..EntryMeta::default()
        };
        for off in [300, -180, 0] {
            let past = stamp_at(now - 3_600, off);
            let future = stamp_at(now + 3_600, off);
            assert!(parse_meta(&format!("---\nvalid_to: {past}\n---\n")).is_ok());
            assert!(meta_expired(&meta(past.clone())), "{past} is an hour ago");
            assert!(
                !meta_expired(&meta(future.clone())),
                "{future} is an hour ahead"
            );
        }
        assert!(
            !meta_expired(&meta("not-a-date".into())),
            "unparseable reads as not expired"
        );
    }

    #[test]
    fn rfc3339_epoch_handles_z_and_offset() {
        let z = rfc3339_epoch("2020-01-01T00:00:00Z").unwrap();
        let off = rfc3339_epoch("2020-01-01T02:00:00+02:00").unwrap();
        assert_eq!(z, off, "the zone offset must shift the epoch");
        assert!(rfc3339_epoch("not-a-date").is_none());
        assert!(rfc3339_epoch("2020-01-02 00:00:00").is_none());
    }

    fn v2_stores(tag: &str) -> Vec<(Scope, PathBuf)> {
        let root = tmpdir().join(tag);
        let stores = vec![
            (Scope::User, root.join("user")),
            (Scope::Project, root.join("project")),
        ];
        for (_, d) in &stores {
            ensure(d).unwrap();
        }
        stores
    }

    #[test]
    fn resident_empty_is_one_short_line() {
        let r = resident(&v2_stores("empty"), 1_790_000_000);
        assert_eq!(r, EMPTY_RESIDENT);
        assert!(r.lines().nth(1).is_some_and(|l| l.len() <= 130), "{r}");
    }

    #[test]
    fn resident_is_byte_stable_ordered_and_path_free() {
        // A fixed clock a month past the files' mtimes: unused notes are
        // old, the one used at NOW is fresh.
        let now = now_secs() + 30 * 86_400;
        let stores = v2_stores("stable");
        let (user, project) = (&stores[0].1, &stores[1].1);
        std::fs::write(user.join(CORE_NAME), "prefers terse replies\n").unwrap();
        std::fs::write(project.join(CORE_NAME), "rust workspace, cargo\n").unwrap();
        for n in ["a", "b", "c"] {
            std::fs::write(project.join(format!("semantic/{n}.md")), format!("# {n}\n")).unwrap();
            append_pointer(project, &format!("semantic/{n}.md — note {n}")).unwrap();
        }
        std::fs::write(user.join("profile/me.md"), "# me\n").unwrap();
        append_pointer(user, "profile/me.md — me").unwrap();
        activation::record(project, "semantic/c.md", now).unwrap();
        let a = resident(&stores, now);
        assert_eq!(a, resident(&stores, now), "same state + clock → same bytes");
        let user_core = a.find("prefers terse").unwrap();
        let project_core = a.find("rust workspace").unwrap();
        let first = a.find("project:semantic/c.md — note c").unwrap();
        let rest = a.find("project:semantic/a.md — note a").unwrap();
        assert!(a.starts_with("## Memory\nUse the memory tool"));
        assert!(
            user_core < project_core && project_core < first && first < rest,
            "{a}"
        );
        assert!(
            a.find("project:semantic/b.md").unwrap() > rest,
            "ties by name"
        );
        assert!(a.contains("user:profile/me.md — me"));
        assert!(
            !a.contains(&*user.parent().unwrap().to_string_lossy()),
            "no absolute paths"
        );
    }

    #[test]
    fn resident_keeps_24_lines_under_4k_with_a_more_tail() {
        let stores = v2_stores("cap");
        let (user, project) = (&stores[0].1, &stores[1].1);
        for i in 0..400 {
            std::fs::write(project.join(format!("semantic/n{i:03}.md")), "x\n").unwrap();
            append_pointer(
                project,
                &format!("semantic/n{i:03}.md — {}", "padding ".repeat(5)),
            )
            .unwrap();
        }
        let r = resident(&stores, 1_790_000_000);
        assert!(r.len() <= RESIDENT_CAP, "{}", r.len());
        let shown = r.lines().filter(|l| l.starts_with("project:")).count();
        assert_eq!(shown, RESIDENT_LINES);
        assert_eq!(r.lines().last().unwrap(), "… 376 more — memory search");
        // Both cores over budget: each capped with its repair note, and
        // the byte cap still holds with the line tail.
        std::fs::write(user.join(CORE_NAME), "u".repeat(3_000)).unwrap();
        std::fs::write(project.join(CORE_NAME), "p".repeat(3_000)).unwrap();
        let r = resident(&stores, 1_790_000_000);
        assert!(r.len() <= RESIDENT_CAP, "{}", r.len());
        assert_eq!(r.matches("exceeds 1500 bytes").count(), 2, "{r}");
        assert!(r.matches('u').count() >= CORE_CAP && r.matches('p').count() >= CORE_CAP);
        let shown = r.lines().filter(|l| l.starts_with("project:")).count();
        assert!(shown < RESIDENT_LINES, "{shown}");
        assert_eq!(
            r.lines().last().unwrap(),
            format!("… {} more — memory search", 400 - shown)
        );
    }

    /// Legacy `--memory` (store inside the workspace) text obeys the same
    /// total cap, with the same `… N more` tail.
    #[test]
    fn legacy_resident_text_shares_the_cap() {
        let root = tmpdir();
        let (ws, user) = (root.join("ws"), root.join("user"));
        let mem = ws.join(".overseer/memory");
        ensure(&mem).unwrap();
        ensure(&user).unwrap();
        std::fs::write(user.join(CORE_NAME), "u".repeat(1_400)).unwrap();
        std::fs::write(mem.join(CORE_NAME), "c".repeat(1_400)).unwrap();
        for i in 0..300 {
            std::fs::write(mem.join(format!("t{i}.md")), "x\n").unwrap();
            append_pointer(&mem, &format!("t{i}.md — legacy topic {i} with words")).unwrap();
        }
        let cfg = crate::agent::AgentConfig {
            cwd: ws.clone(),
            memory_dir: Some(mem.clone()),
            user_memory_dir: Some(user),
            ..crate::agent::AgentConfig::default()
        };
        let seg = resident_segment(&cfg).unwrap();
        assert!(seg.len() <= RESIDENT_CAP, "{}", seg.len());
        assert!(seg.contains("## Memory index") && seg.contains("t0.md — legacy topic 0"));
        assert!(seg.contains("more — memory search"), "{seg}");
        assert!(seg.contains("## user core"), "{seg}");
    }
}
