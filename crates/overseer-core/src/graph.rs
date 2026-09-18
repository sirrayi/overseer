//! Graph-memory patterns ported from the memory-graph batch (arsenal B2).
//!
//! Two ports, both offline and dependency-free — no graph database, no
//! server, no model call:
//!
//! - **graphiti `graph.jsonl`** — a temporal knowledge graph persisted as
//!   an *append-only* JSONL edge log. Every edge carries the fact text, its
//!   provenance, and the half-open interval it was true in, so "what did we
//!   believe at time `t`?" is a pure filter over a file nobody has to
//!   migrate: a correction appends a new edge with a new interval and
//!   closes the old one, and no line is ever rewritten. That is what makes
//!   the log auditable — the history of belief is the file itself
//!   (`Edge`, `parse_line`, `append`, `load`, `live_at`, `neighbours`).
//! - **graphrag offline summary** — `architecture_summary` is the
//!   community-report stand-in: a deterministic markdown digest of the
//!   whole log (entities with degrees, relation kinds with counts, the time
//!   span) that costs no model call, so an agent can orient in a large
//!   graph without reading all of it. It is derived, never the truth: the
//!   log is the source of truth, and the same edges must always produce
//!   byte-identical bytes.
//!
//! `// DEFERRED(owner): graphiti's LLM extraction pipeline (entity
//! resolution, edge extraction from prose) and its search rerankers, plus
//! graphrag's community detection and model-written community reports —
//! this port lands the storage format, the temporal query, and the
//! deterministic offline report; everything that needs a model stays out.`

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

/// Cap on the bytes `architecture_summary` may emit. Over the cap the body
/// is truncated *with a repair note*, never silently — a reader must always
/// be able to tell a short graph from a clipped report.
pub const SUMMARY_CAP: usize = 8_000;

// ── graphiti: the append-only temporal edge log ──────────────────────────

/// One edge of the temporal graph, exactly as graphiti's `graph.jsonl`
/// stores it: `from`/`to` name the endpoints (entities or concepts), `kind`
/// the relation, `fact` the sentence a reader gets back, `source` the
/// provenance the fact came from, and `valid_from`/`valid_to` the interval
/// the fact is asserted true in.
///
/// Invariants, all enforced by [`validate_edge`] — the one gate both
/// [`parse_line`] and [`append`] run, so a line the log accepts is a line
/// the log will write:
/// - every field except `valid_to` is present and non-empty after trimming
///   (an unnamed endpoint or a blank `fact` is a line no reader can use);
/// - `valid_from` and `valid_to` are UTC `Z` stamps ([`valid_utc_stamp`]);
/// - `valid_to`, when present, is strictly after `valid_from`: the interval
///   is half-open, and a zero-length one holds nothing.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Edge {
    pub from: String,
    pub to: String,
    pub kind: String,
    pub fact: String,
    pub source: String,
    pub valid_from: String,
    /// Absent/`null` means the fact is still believed (an open interval).
    #[serde(default)]
    pub valid_to: Option<String>,
}

impl Edge {
    /// Is this edge's interval live at `t`? The interval is half-open
    /// `[valid_from, valid_to)`: `t == valid_from` is live, `t == valid_to`
    /// is **not**, and `None` means the fact is still true.
    ///
    /// The exclusive end is what makes a correction exact. Retiring a fact
    /// at time `T` writes `old.valid_to = T` and `new.valid_from = T`, so
    /// every instant has exactly one live edge — with an inclusive end the
    /// instant `T` would have two, and a query would be ambiguous at
    /// precisely the moment the correction happened.
    ///
    /// Ordering is text ordering over validated stamps, which is exact
    /// because the seconds prefix `YYYY-MM-DDTHH:MM:SS` is fixed-width —
    /// see [`stamp_key`] for how the optional sub-second fraction is folded
    /// in so `.5` and `.500` are the same instant, not two byte orders.
    ///
    /// A `t` that is not a UTC stamp, or a hand-built edge whose own
    /// `valid_from`/`valid_to` do not validate, is never live: the filter
    /// fails closed, so garbage in the clock cannot resurrect a retired
    /// fact. Stored edges cannot take that path — the gate rejects them
    /// before they reach the file.
    pub fn live_at(&self, t: &str) -> bool {
        let Some(now) = stamp_key(t) else {
            return false;
        };
        let Some(start) = stamp_key(&self.valid_from) else {
            return false;
        };
        if now < start {
            return false;
        }
        match &self.valid_to {
            None => true,
            Some(end) => match stamp_key(end) {
                Some(end) => now < end,
                None => false,
            },
        }
    }
}

/// Structural UTC timestamp check for the one form graphiti writes:
/// `YYYY-MM-DDTHH:MM:SS[.fraction]Z`.
///
/// Ranges are validated for real — month `1..=12`, day `1..=days_in_month`
/// of that month with leap years (divisible by 4, except centuries not
/// divisible by 400), hour `0..=23`, minute and second `0..=59` — so a
/// stamp that passes can be compared as text without a date library. The
/// year is taken at face value; there is no calendar-start policy here.
/// Leap second `:60` is rejected on purpose: it would break the fixed-width
/// text ordering that [`Edge::live_at`] relies on.
///
/// Deliberately *narrower* than `memory`'s general RFC3339 validator: the
/// log stores UTC only, so an offset form (`+02:00`), a lowercase `t`/`z`
/// separator, a space instead of `T`, and a date with no zone at all are
/// all rejected here. Convert to UTC at the boundary rather than store a
/// local time whose ordering depends on an offset the endpoint forgot to
/// normalize.
pub fn valid_utc_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 20 {
        return false;
    }
    let Some(year) = digits(b, 0, 4) else {
        return false;
    };
    if b.get(4) != Some(&b'-') {
        return false;
    }
    let Some(month) = digits(b, 5, 2) else {
        return false;
    };
    if b.get(7) != Some(&b'-') {
        return false;
    }
    let Some(day) = digits(b, 8, 2) else {
        return false;
    };
    if b.get(10) != Some(&b'T') {
        return false;
    }
    let Some(hour) = digits(b, 11, 2) else {
        return false;
    };
    if b.get(13) != Some(&b':') {
        return false;
    }
    let Some(minute) = digits(b, 14, 2) else {
        return false;
    };
    if b.get(16) != Some(&b':') {
        return false;
    }
    let Some(second) = digits(b, 17, 2) else {
        return false;
    };
    let mut i = 19;
    if b.get(i) == Some(&b'.') {
        i += 1;
        let start = i;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    // Exactly `Z`, and nothing after it.
    if b.get(i) != Some(&b'Z') || i + 1 != b.len() {
        return false;
    }
    if month == 0 || month > 12 || day == 0 || day > days_in_month(year, month) {
        return false;
    }
    hour <= 23 && minute <= 59 && second <= 59
}

/// `n` ASCII digits at `at`, as a number; `None` when they are not all
/// digits (so a caller can tell "not a stamp" from "0").
fn digits(b: &[u8], at: usize, n: usize) -> Option<u32> {
    let slice = b.get(at..at + n)?;
    let mut v = 0u32;
    for c in slice {
        if !c.is_ascii_digit() {
            return None;
        }
        v = v * 10 + u32::from(c - b'0');
    }
    Some(v)
}

fn is_leap(year: u32) -> bool {
    year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400))
}

/// Days in a calendar month; `0` for a month outside `1..=12`, which makes
/// the day check reject an impossible month without a second branch.
fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Comparable key for a validated stamp: `(seconds prefix, fraction)`.
///
/// The seconds prefix is fixed-width, so comparing it as text *is*
/// comparing it as time. The fraction is trimmed of trailing zeros before
/// comparison, which is what keeps the ordering exact when one writer emits
/// milliseconds and another microseconds: as raw bytes `…00Z` would sort
/// *after* `…00.5Z`, but `.5`, `.500`, and `.0005` all compare as the same
/// instant here and `.5 < .51` exactly as the numbers do.
///
/// `None` for a stamp that does not validate — the caller decides whether
/// that means "not live" (the query) or "broken edge" (the gate).
fn stamp_key(s: &str) -> Option<(&str, &str)> {
    if !valid_utc_stamp(s) {
        return None;
    }
    let secs = s.get(..19)?; // ASCII by validation, so this cannot split a char.
    let frac = match s.get(20..s.len() - 1) {
        Some(f) if !f.is_empty() => f.trim_end_matches('0'),
        _ => "",
    };
    Some((secs, frac))
}

/// The one gate on an edge: every required field present and non-empty
/// (whitespace-only counts as empty), both stamps valid UTC, and
/// `valid_to` — when present — strictly after `valid_from`.
///
/// Returns the bare reason (no `graph:` prefix) so [`parse_line`] and
/// [`load`] can attach the line number and [`append`] the path without
/// repeating the prefix. Every message names the offending field and what
/// to write instead, because this error is the only thing standing between
/// a corrupt log and every later query.
pub fn validate_edge(edge: &Edge) -> Result<(), String> {
    for (name, value) in [
        ("from", &edge.from),
        ("to", &edge.to),
        ("kind", &edge.kind),
        ("fact", &edge.fact),
        ("source", &edge.source),
        ("valid_from", &edge.valid_from),
    ] {
        if value.trim().is_empty() {
            return Err(format!(
                "field `{name}` is empty — every edge must name both endpoints, the relation \
                 kind, the fact text, its source, and when it became true"
            ));
        }
    }
    if !valid_utc_stamp(&edge.valid_from) {
        return Err(format!(
            "`valid_from` = `{}` is not a UTC stamp — want YYYY-MM-DDTHH:MM:SS[.fraction]Z \
             (convert local times at the boundary)",
            edge.valid_from
        ));
    }
    if let Some(to) = &edge.valid_to {
        if !valid_utc_stamp(to) {
            return Err(format!(
                "`valid_to` = `{to}` is not a UTC stamp — want YYYY-MM-DDTHH:MM:SS[.fraction]Z, \
                 or omit the field entirely for an open interval"
            ));
        }
        match (stamp_key(&edge.valid_from), stamp_key(to)) {
            (Some(start), Some(end)) if end > start => {}
            _ => {
                return Err(format!(
                    "`valid_to` = `{to}` is not after `valid_from` = `{}` — the interval is \
                     half-open, so its end must be strictly later; omit `valid_to` if the fact \
                     is still true",
                    edge.valid_from
                ));
            }
        }
    }
    Ok(())
}

/// Parse one JSONL line into an [`Edge`], running the shared gate.
/// Reasons are bare (no `graph:` prefix, no line number) — that context
/// belongs to [`parse_line`] and [`load`].
fn parse_object(line: &str) -> Result<Edge, String> {
    let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
        format!(
            "line is not JSON ({e}) — one JSON object per line, e.g. {{\"from\":\"ada\",\
             \"to\":\"engine\",\"kind\":\"designs\",\"fact\":\"ada designs the engine\",\
             \"source\":\"chat\",\"valid_from\":\"2026-01-02T15:04:05Z\"}}"
        )
    })?;
    let obj = value.as_object().ok_or_else(|| {
        "line must be a JSON object, not a bare value — one object per line, one edge per object"
            .to_string()
    })?;
    fn text(
        obj: &serde_json::Map<String, serde_json::Value>,
        name: &str,
    ) -> Result<String, String> {
        match obj.get(name) {
            None => Err(format!(
                "field `{name}` is missing — every edge must name both endpoints, the relation \
                 kind, the fact text, its source, and when it became true"
            )),
            Some(serde_json::Value::String(s)) => Ok(s.clone()),
            Some(other) => Err(format!(
                "field `{name}` must be a JSON string, got `{other}` — quote the value"
            )),
        }
    }
    let edge = Edge {
        from: text(obj, "from")?,
        to: text(obj, "to")?,
        kind: text(obj, "kind")?,
        fact: text(obj, "fact")?,
        source: text(obj, "source")?,
        valid_from: text(obj, "valid_from")?,
        valid_to: match obj.get("valid_to") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(other) => {
                return Err(format!(
                    "field `valid_to` must be a JSON string or null, got `{other}` — use null \
                     (or omit it) for an open interval"
                ));
            }
        },
    };
    validate_edge(&edge)?;
    Ok(edge)
}

/// Parse one line of the log. The gate is [`validate_edge`], so this
/// accepts exactly what [`append`] writes and nothing else — a hand-edited
/// line that fails here would never have been written by the engine.
///
/// `valid_to` may be absent or `null` (both mean an open interval); an
/// empty string is refused rather than read as "no end", because a blank
/// stamp is a truncated value, not a decision.
pub fn parse_line(line: &str) -> Result<Edge, String> {
    parse_object(line).map_err(|e| format!("graph: {e}"))
}

/// Append one validated edge as a single JSONL line, creating the file when
/// absent. Validation runs *first*, so a rejected edge never reaches the
/// file: the log only ever grows by lines [`parse_line`] accepts, and a
/// caller that ignores the error still has an unchanged log.
///
/// JSON escaping keeps a `fact` containing newlines on one line, so "one
/// edge = one line" holds for any text the model writes, not just tidy
/// prose. Existing content is never rewritten or reordered — that is the
/// whole point of the format.
pub fn append(path: &Path, edge: &Edge) -> Result<(), String> {
    validate_edge(edge).map_err(|e| format!("graph: refusing to append: {e}"))?;
    let mut line =
        serde_json::to_string(edge).map_err(|e| format!("graph: cannot serialize edge: {e}"))?;
    line.push('\n');
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map_err(|e| format!("graph: cannot open `{}` to append: {e}", path.display()))?;
    file.write_all(line.as_bytes())
        .map_err(|e| format!("graph: cannot write `{}`: {e}", path.display()))
}

/// Load the whole log in file order. Blank lines are skipped (a trailing
/// newline is not an error, and neither is a gap a hand-edit left), but a
/// malformed line is an `Err` naming its 1-based line number and the
/// reason: silently dropping a bad line would make a fact vanish from every
/// later query with nothing on screen to explain it.
///
/// A missing file is an `Err` too — an empty log and no log are different
/// facts, and only the caller knows which one is fine.
pub fn load(path: &Path) -> Result<Vec<Edge>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("graph: cannot read `{}`: {e}", path.display()))?;
    let mut edges = Vec::new();
    for (idx, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let edge = parse_object(line)
            .map_err(|e| format!("graph: line {} of `{}`: {e}", idx + 1, path.display()))?;
        edges.push(edge);
    }
    Ok(edges)
}

/// The temporal query: every edge live at `t`, in input (log) order. Order
/// is preserved rather than sorted because the caller asked what the log
/// said; the log's order is the order belief was written down, and a
/// consumer that wants recency can sort on `valid_from` itself.
pub fn live_at<'a>(edges: &'a [Edge], t: &str) -> Vec<&'a Edge> {
    edges.iter().filter(|e| e.live_at(t)).collect()
}

/// Distinct counterpart names reachable from `node` at `t`, sorted.
///
/// Direction is deliberately ignored: "everything this entity is connected
/// to right now" is the question an agent about to write a new edge asks,
/// and an incoming relation links the two concepts just as tightly as an
/// outgoing one. Names are deduped (two kinds between the same pair are one
/// neighbour) and sorted so the same graph always yields the same list.
pub fn neighbours(edges: &[Edge], node: &str, t: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for edge in live_at(edges, t) {
        if edge.from == node {
            out.push(edge.to.clone());
        }
        if edge.to == node {
            out.push(edge.from.clone());
        }
    }
    out.sort();
    out.dedup();
    out
}

// ── graphrag: the deterministic offline summary ──────────────────────────

/// Deterministic markdown digest of the whole log — graphrag's community
/// report with the model taken out. Three sections:
///
/// - `## Entities (N)` — one line per distinct name, `degree D (out:o
///   in:i)`, sorted by degree descending then name ascending. A name that
///   appears only as `to` still counts as an entity; a self-loop is one
///   edge out and one edge in, so its degree is 2;
/// - `## Relations (N)` — one line per distinct kind, `count C`, sorted by
///   count descending then kind ascending;
/// - `## Span` — the earliest `valid_from` and the latest upper bound
///   (`open (no upper bound)` when any edge has `valid_to: None`, since the
///   graph's horizon is then unbounded).
///
/// The report covers the *whole* log, not a time slice, so counts include
/// duplicated and retired edges — pass `live_at` results in when a snapshot
/// is what is wanted. Ordering is total (ties break on the name), so the
/// same edges produce byte-identical output no matter what order they were
/// appended in; that property is what makes the digest cacheable and
/// diffable.
///
/// Over [`SUMMARY_CAP`] the body is truncated at a whole line and an
/// explicit repair note is appended — never a silent cut. Edges built by
/// hand with invalid stamps are ignored for the span rather than panicking
/// or inventing a bound; the section says `none` when nothing is left.
pub fn architecture_summary(edges: &[Edge]) -> String {
    let mut degrees: BTreeMap<&str, (u64, u64)> = BTreeMap::new();
    let mut kinds: BTreeMap<&str, u64> = BTreeMap::new();
    let mut earliest: Option<(&str, (&str, &str))> = None;
    let mut latest: Option<(&str, (&str, &str))> = None;
    let mut open_end = false;

    for edge in edges {
        let entry = degrees.entry(edge.from.as_str()).or_default();
        entry.0 += 1;
        degrees.entry(edge.to.as_str()).or_default().1 += 1;
        *kinds.entry(edge.kind.as_str()).or_default() += 1;
        if let Some(key) = stamp_key(&edge.valid_from) {
            if earliest.is_none_or(|(_, best)| key < best) {
                earliest = Some((edge.valid_from.as_str(), key));
            }
        }
        match &edge.valid_to {
            None => open_end = true,
            Some(end) => {
                if let Some(key) = stamp_key(end) {
                    if latest.is_none_or(|(_, best)| key > best) {
                        latest = Some((end.as_str(), key));
                    }
                }
            }
        }
    }

    let mut entities: Vec<(&str, u64, u64)> = degrees
        .iter()
        .map(|(name, (out, inc))| (*name, *out, *inc))
        .collect();
    entities.sort_by(|a, b| (b.1 + b.2).cmp(&(a.1 + a.2)).then_with(|| a.0.cmp(b.0)));
    let mut relations: Vec<(&str, u64)> = kinds.iter().map(|(k, c)| (*k, *c)).collect();
    relations.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(b.0)));

    let mut body = String::from("# Graph architecture\n\n");
    body.push_str(&format!("## Entities ({})\n", entities.len()));
    for (name, out, inc) in &entities {
        body.push_str(&format!(
            "- {name} — degree {} (out:{out} in:{inc})\n",
            out + inc
        ));
    }
    body.push_str(&format!("\n## Relations ({})\n", relations.len()));
    for (kind, count) in &relations {
        body.push_str(&format!("- {kind} — count {count}\n"));
    }
    body.push_str("\n## Span\n");
    match earliest {
        Some((stamp, _)) => body.push_str(&format!("- earliest: {stamp}\n")),
        None => body.push_str("- earliest: none\n"),
    }
    match (open_end, latest) {
        (true, _) => body.push_str("- latest: open (no upper bound)\n"),
        (false, Some((stamp, _))) => body.push_str(&format!("- latest: {stamp} (closed)\n")),
        (false, None) => body.push_str("- latest: none\n"),
    }

    if body.len() <= SUMMARY_CAP {
        return body;
    }
    let cut = line_boundary(&body, SUMMARY_CAP);
    let mut out = body[..cut].trim_end().to_string();
    out.push_str(&format!(
        "\n\n[overseer] graph summary exceeds {SUMMARY_CAP} bytes — the tail is truncated \
         ({} entities, {} relations in the log). Query the log instead of reading it: \
         `live_at(t)` for a snapshot, `neighbours(node, t)` for one entity's links.\n",
        entities.len(),
        relations.len()
    ));
    out
}

/// Largest char boundary at or below `cap`, backed up to the start of the
/// next line so a truncated report never ends mid-line (and never mid-char
/// — the cut is by chars, never bytes).
fn line_boundary(body: &str, cap: usize) -> usize {
    let mut cut = body
        .char_indices()
        .map(|(i, c)| i + c.len_utf8())
        .take_while(|end| *end <= cap)
        .last()
        .unwrap_or(0);
    if let Some(nl) = body[..cut].rfind('\n') {
        cut = nl + 1;
    }
    cut
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos();
        let d = std::env::temp_dir().join(format!(
            "p8-graph-{}-{nanos}-{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A well-formed edge, with the fields the tests then break on purpose.
    fn mk(from: &str, to: &str, kind: &str, valid_from: &str, valid_to: Option<&str>) -> Edge {
        Edge {
            from: from.to_string(),
            to: to.to_string(),
            kind: kind.to_string(),
            fact: format!("{from} {kind} {to}"),
            source: "chat".to_string(),
            valid_from: valid_from.to_string(),
            valid_to: valid_to.map(str::to_string),
        }
    }

    const LINE: &str = r#"{"from":"ada","to":"engine","kind":"designs","fact":"ada designs the engine","source":"chat","valid_from":"2026-01-02T15:04:05Z"}"#;

    #[test]
    fn valid_utc_stamp_enforces_ranges_and_full_width() {
        for good in [
            "2026-01-02T15:04:05Z",
            "2026-01-02T15:04:05.123Z",
            "2026-01-02T15:04:05.123456Z",
            "2024-02-29T00:00:00Z",
            "2000-02-29T23:59:59Z",
        ] {
            assert!(valid_utc_stamp(good), "{good} is a valid UTC stamp");
        }
        for bad in [
            "2023-02-29T00:00:00Z",      // not a leap year
            "2100-02-29T00:00:00Z",      // century, not leap
            "2026-13-01T00:00:00Z",      // month range
            "2026-00-10T00:00:00Z",      // month range
            "2026-01-00T00:00:00Z",      // day range
            "2026-04-31T00:00:00Z",      // day range for a 30-day month
            "2026-01-02T24:00:00Z",      // hour range
            "2026-01-02T15:60:00Z",      // minute range
            "2026-01-02T15:04:60Z",      // leap second breaks text ordering
            "2026-01-02T15:04:05",       // no zone
            "2026-01-02T15:04:05.Z",     // empty fraction
            "2026-01-02 15:04:05Z",      // space instead of T
            "2026-01-02t15:04:05z",      // lowercase separators
            "2026-1-02T15:04:05Z",       // short field
            "2026-01-02T15:04:05+00:00", // offset form: the log is UTC-only
            "2026-01-02T15:04:05Zjunk",  // trailing garbage
            "",
        ] {
            assert!(!valid_utc_stamp(bad), "{bad} must be rejected");
        }
    }

    #[test]
    fn parse_line_rejects_missing_and_empty_required_fields() {
        let base: serde_json::Value = serde_json::from_str(LINE).unwrap();
        assert!(parse_line(LINE).is_ok(), "the fixture itself must parse");
        for field in ["from", "to", "kind", "fact", "source", "valid_from"] {
            let mut missing = base.clone();
            missing.as_object_mut().unwrap().remove(field);
            let err = parse_line(&missing.to_string()).unwrap_err();
            assert!(err.contains(field), "missing `{field}` must name it: {err}");

            let mut blank = base.clone();
            blank[field] = serde_json::Value::String("   ".to_string());
            let err = parse_line(&blank.to_string()).unwrap_err();
            assert!(
                err.contains(field) && err.contains("empty"),
                "whitespace-only `{field}` must be refused: {err}"
            );
        }
        // A non-string field is refused, not coerced to text.
        let mut wrong = base.clone();
        wrong["kind"] = serde_json::json!(7);
        let err = parse_line(&wrong.to_string()).unwrap_err();
        assert!(err.contains("kind") && err.contains("string"), "{err}");
        // A line that is not an object at all is refused too.
        assert!(parse_line("[1,2]").unwrap_err().contains("JSON object"));
        assert!(parse_line("not json").unwrap_err().contains("not JSON"));
    }

    #[test]
    fn parse_line_rejects_offset_stamps_and_non_positive_intervals() {
        let offset = LINE.replace("2026-01-02T15:04:05Z", "2026-01-02T15:04:05+02:00");
        let err = parse_line(&offset).unwrap_err();
        assert!(
            err.contains("valid_from"),
            "the narrower gate names it: {err}"
        );

        let with = |v: &str| LINE.replace(r#""source":"chat""#, &format!(r#""source":"chat",{v}"#));
        for bad in [
            r#""valid_to":"2026-01-02T15:04:05Z""#, // equal to valid_from
            r#""valid_to":"2026-01-01T00:00:00Z""#, // before valid_from
            r#""valid_to":"""#,                     // blank, not a decision
            r#""valid_to":42"#,                     // not a string
        ] {
            let err = parse_line(&with(bad)).unwrap_err();
            assert!(err.contains("valid_to"), "`{bad}` must be refused: {err}");
        }
        // null and an absent field are both the open interval.
        assert_eq!(
            parse_line(&with(r#""valid_to":null"#)).unwrap().valid_to,
            None
        );
        assert_eq!(parse_line(LINE).unwrap().valid_to, None);
        assert_eq!(
            parse_line(&with(r#""valid_to":"2026-02-01T00:00:00Z""#))
                .unwrap()
                .valid_to
                .as_deref(),
            Some("2026-02-01T00:00:00Z")
        );
    }

    #[test]
    fn live_at_end_is_exclusive_and_none_stays_open() {
        let closed = mk(
            "a",
            "b",
            "k",
            "2026-01-01T00:00:00Z",
            Some("2026-02-01T00:00:00Z"),
        );
        assert!(
            closed.live_at("2026-01-01T00:00:00Z"),
            "valid_from is inside"
        );
        assert!(closed.live_at("2026-01-31T23:59:59Z"));
        assert!(
            !closed.live_at("2026-02-01T00:00:00Z"),
            "valid_to is exclusive: a correction at T leaves one live edge"
        );
        assert!(!closed.live_at("2025-12-31T23:59:59Z"));

        let open = mk("a", "b", "k", "2026-01-01T00:00:00Z", None);
        assert!(
            open.live_at("2099-12-31T23:59:59Z"),
            "None means still true"
        );
        assert!(!open.live_at("2025-12-31T23:59:59Z"));

        // The filter fails closed: a hand-built edge with a bad stamp is
        // never live, and a malformed clock reads as nothing, never a panic.
        let broken = mk("a", "b", "k", "yesterday", None);
        assert!(!broken.live_at("2026-01-01T00:00:00Z"));
        assert!(!closed.live_at("not a stamp"));
    }

    #[test]
    fn live_at_orders_mixed_fraction_widths_by_value() {
        let e = mk(
            "a",
            "b",
            "k",
            "2026-01-01T00:00:00Z",
            Some("2026-01-01T00:00:00.5Z"),
        );
        assert!(e.live_at("2026-01-01T00:00:00.3Z"));
        assert!(e.live_at("2026-01-01T00:00:00.000Z"));
        assert!(
            !e.live_at("2026-01-01T00:00:00.500Z"),
            ".5 and .500 are the same instant, so the exclusive end excludes it"
        );
        assert!(
            !e.live_at("2026-01-01T00:00:00.7Z"),
            "raw bytes would say live"
        );

        let wide = mk(
            "a",
            "b",
            "k",
            "2026-01-01T00:00:00.5Z",
            Some("2026-01-01T00:00:00.51Z"),
        );
        assert!(wide.live_at("2026-01-01T00:00:00.500Z"), ".500 == .5");
        assert!(wide.live_at("2026-01-01T00:00:00.505Z"));
        assert!(!wide.live_at("2026-01-01T00:00:00.51Z"));
    }

    #[test]
    fn append_then_load_round_trips_every_field_in_order() {
        let dir = tmpdir();
        let path = dir.join("graph.jsonl");
        let a = mk("ada", "engine", "designs", "2026-01-02T15:04:05Z", None);
        let b = mk(
            "engine",
            "ada",
            "owes",
            "2026-01-03T00:00:00.250Z",
            Some("2026-02-01T00:00:00Z"),
        );
        assert!(append(&path, &a).is_ok(), "fresh append creates the log");
        append(&path, &b).unwrap();
        let back = load(&path).unwrap();
        assert_eq!(back, vec![a.clone(), b.clone()], "every field survives");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().lines().count(),
            2,
            "one edge is one line"
        );

        // A fact with newlines still occupies exactly one line (JSON
        // escaping is what keeps "one edge = one line" true for any text).
        let mut multiline = a.clone();
        multiline.fact = "line one\nline two".to_string();
        append(&path, &multiline).unwrap();
        assert_eq!(load(&path).unwrap().len(), 3);
        assert_eq!(std::fs::read_to_string(&path).unwrap().lines().count(), 3);
        assert_eq!(load(&path).unwrap()[2].fact, multiline.fact);
    }

    #[test]
    fn append_refuses_invalid_edges_and_writes_nothing() {
        let dir = tmpdir();
        let path = dir.join("graph.jsonl");
        let mut bad = mk("ada", "engine", "designs", "2026-01-02T15:04:05Z", None);
        bad.kind = "  ".to_string();
        let err = append(&path, &bad).unwrap_err();
        assert!(err.contains("kind"), "{err}");
        assert!(!path.exists(), "a rejected edge must not create the log");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

        append(
            &path,
            &mk("ada", "engine", "designs", "2026-01-02T15:04:05Z", None),
        )
        .unwrap();
        let before = std::fs::read_to_string(&path).unwrap();
        bad.valid_from = "2026-01-02T15:04:05+02:00".to_string();
        assert!(append(&path, &bad).is_err(), "bad stamp refused");
        let reversed = mk(
            "a",
            "b",
            "k",
            "2026-02-01T00:00:00Z",
            Some("2026-01-01T00:00:00Z"),
        );
        assert!(
            append(&path, &reversed).is_err(),
            "reversed interval refused"
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            before,
            "a refused append leaves the log byte-identical"
        );
        assert_eq!(load(&path).unwrap().len(), 1);
    }

    #[test]
    fn load_reports_the_offending_line_number_and_missing_file_is_an_error() {
        let dir = tmpdir();
        let path = dir.join("graph.jsonl");
        let good = serde_json::to_string(&mk(
            "ada",
            "engine",
            "designs",
            "2026-01-02T15:04:05Z",
            None,
        ))
        .unwrap();
        std::fs::write(&path, format!("{good}\n\n{good}\n{{ not json }}\n{good}\n")).unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains("line 4"), "the line number is named: {err}");
        assert!(err.contains("not JSON"), "the reason rides along: {err}");
        assert!(err.contains(path.to_str().unwrap()), "{err}");

        // Blank lines are skipped, never counted as edges.
        std::fs::write(&path, format!("{good}\n\n\n")).unwrap();
        assert_eq!(load(&path).unwrap().len(), 1);

        // The first line is line 1, and a field fault is named with it.
        std::fs::write(&path, format!("{{}}\n{good}\n")).unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.contains("line 1") && err.contains("from"), "{err}");

        // Missing file: an error naming the path, not an empty log.
        let err = load(&dir.join("absent.jsonl")).unwrap_err();
        assert!(err.contains("absent.jsonl"), "{err}");
    }

    #[test]
    fn live_at_query_keeps_log_order_and_skips_retired_edges() {
        let e1 = mk(
            "a",
            "b",
            "k",
            "2026-01-01T00:00:00Z",
            Some("2026-03-01T00:00:00Z"),
        );
        let e2 = mk(
            "c",
            "d",
            "k",
            "2026-02-01T00:00:00Z",
            Some("2026-02-02T00:00:00Z"),
        );
        let e3 = mk("e", "f", "k", "2026-01-15T00:00:00Z", None);
        let edges = vec![e3.clone(), e1.clone(), e2.clone()];
        assert_eq!(
            live_at(&edges, "2026-01-20T00:00:00Z"),
            vec![&e3, &e1],
            "log order preserved; e2 has not started yet"
        );
        assert_eq!(
            live_at(&edges, "2026-02-01T12:00:00Z"),
            vec![&e3, &e1, &e2],
            "order is the log's, not valid_from order"
        );
        assert_eq!(live_at(&edges, "2026-06-01T00:00:00Z"), vec![&e3]);
        assert!(live_at(&edges, "garbage").is_empty());
    }

    #[test]
    fn neighbours_dedupes_sorts_and_ignores_non_live_edges() {
        let edges = vec![
            mk("ada", "engine", "designs", "2026-01-01T00:00:00Z", None),
            mk("ada", "engine", "reviews", "2026-01-01T00:00:00Z", None),
            mk("grace", "ada", "mentors", "2026-01-01T00:00:00Z", None),
            mk(
                "ada",
                "zoe",
                "knows",
                "2026-01-01T00:00:00Z",
                Some("2026-01-02T00:00:00Z"),
            ),
            mk(
                "ada",
                "older",
                "knows",
                "2025-01-01T00:00:00Z",
                Some("2025-02-01T00:00:00Z"),
            ),
            mk("ada", "future", "knows", "2027-01-01T00:00:00Z", None),
        ];
        assert_eq!(
            neighbours(&edges, "ada", "2026-01-05T00:00:00Z"),
            vec!["engine".to_string(), "grace".to_string()],
            "two kinds between the same pair are one neighbour; retired and \
             not-yet-true edges are out"
        );
        assert_eq!(
            neighbours(&edges, "ada", "2026-01-01T12:00:00Z"),
            vec!["engine".to_string(), "grace".to_string(), "zoe".to_string()]
        );
        assert_eq!(
            neighbours(&edges, "engine", "2026-01-05T00:00:00Z"),
            vec!["ada".to_string()],
            "direction does not matter"
        );
        assert!(neighbours(&edges, "nobody", "2026-01-05T00:00:00Z").is_empty());
    }

    #[test]
    fn architecture_summary_degrees_and_relation_counts_are_exact() {
        let edges = vec![
            mk("ada", "engine", "designs", "2026-01-01T00:00:00Z", None),
            mk("ada", "engine", "reviews", "2026-01-02T00:00:00Z", None),
            mk("grace", "ada", "mentors", "2026-01-03T00:00:00Z", None),
        ];
        let s = architecture_summary(&edges);
        assert!(s.starts_with("# Graph architecture\n"), "{s}");
        assert!(s.contains("## Entities (3)\n"), "{s}");
        assert!(s.contains("- ada — degree 3 (out:2 in:1)\n"), "{s}");
        assert!(s.contains("- engine — degree 2 (out:0 in:2)\n"), "{s}");
        assert!(s.contains("- grace — degree 1 (out:1 in:0)\n"), "{s}");
        assert!(s.contains("## Relations (3)\n"), "{s}");
        assert!(
            s.contains("- designs — count 1\n- mentors — count 1\n- reviews — count 1\n"),
            "equal counts break on kind asc: {s}"
        );
        assert!(
            s.contains(
                "## Span\n- earliest: 2026-01-01T00:00:00Z\n- latest: open (no upper bound)\n"
            ),
            "{s}"
        );
        assert!(
            !s.contains("[overseer]"),
            "a three-edge graph is not clipped"
        );

        // Order is degree desc then name asc, so a shuffled log (and a
        // re-appended one) renders the same bytes.
        let mut shuffled = edges.clone();
        shuffled.reverse();
        assert_eq!(architecture_summary(&shuffled), s);

        // A closed graph reports its latest valid_to as the upper bound.
        let closed = vec![
            mk(
                "a",
                "b",
                "k",
                "2026-01-01T00:00:00Z",
                Some("2026-02-01T00:00:00Z"),
            ),
            mk(
                "b",
                "c",
                "k",
                "2026-01-05T00:00:00Z",
                Some("2026-01-10T00:00:00Z"),
            ),
        ];
        let c = architecture_summary(&closed);
        assert!(
            c.contains(
                "- earliest: 2026-01-01T00:00:00Z\n- latest: 2026-02-01T00:00:00Z (closed)\n"
            ),
            "{c}"
        );

        // A self-loop is one out and one in, and an empty log still renders.
        let looped = architecture_summary(&[mk("a", "a", "k", "2026-01-01T00:00:00Z", None)]);
        assert!(looped.contains("- a — degree 2 (out:1 in:1)\n"), "{looped}");
        let empty = architecture_summary(&[]);
        assert!(empty.contains("## Entities (0)"), "{empty}");
        assert!(empty.contains("## Relations (0)"), "{empty}");
        assert!(
            empty.contains("- earliest: none\n- latest: none\n"),
            "{empty}"
        );
    }

    #[test]
    fn architecture_summary_is_byte_identical_across_calls() {
        let edges: Vec<Edge> = (0..40)
            .map(|i| {
                mk(
                    &format!("n{}", i % 7),
                    &format!("n{}", (i * 3) % 11),
                    if i % 2 == 0 { "links" } else { "cites" },
                    "2026-01-01T00:00:00Z",
                    None,
                )
            })
            .collect();
        let a = architecture_summary(&edges);
        let b = architecture_summary(&edges);
        assert_eq!(a, b, "same input → byte-identical output");
        assert_eq!(a.as_bytes(), b.as_bytes());
        assert!(a.contains("## Entities (11)\n"), "{a}");
        assert!(a.contains("## Relations (2)\n"), "{a}");
    }

    #[test]
    fn summary_notes_truncation_only_over_the_cap() {
        let small = vec![mk("ada", "engine", "designs", "2026-01-01T00:00:00Z", None)];
        let s = architecture_summary(&small);
        assert!(!s.contains("[overseer]"), "under the cap there is no note");
        assert!(s.len() < SUMMARY_CAP);

        // Multi-byte names near the cut prove the truncation is by chars
        // and by whole lines, never by bytes mid-character.
        let big: Vec<Edge> = (0..900)
            .map(|i| {
                mk(
                    &format!("entité-{i}"),
                    &format!("cible-{i}"),
                    "relie",
                    "2026-01-01T00:00:00Z",
                    None,
                )
            })
            .collect();
        let b = architecture_summary(&big);
        assert!(b.starts_with("# Graph architecture\n"), "{b}");
        assert!(
            b.contains("[overseer]"),
            "over the cap the note is mandatory"
        );
        assert!(b.contains("exceeds"), "the note names the cap: {b}");
        assert!(b.len() > SUMMARY_CAP, "the untruncated body was longer");
        assert!(
            b.len() < SUMMARY_CAP + 1_000,
            "the note is a line, not a second report"
        );
        assert!(
            b.lines().last().unwrap().starts_with("[overseer]"),
            "the note is its own line: {:?}",
            b.lines().last()
        );
        let body = b.split("\n\n[overseer]").next().unwrap();
        assert!(body.len() <= SUMMARY_CAP, "the kept body respects the cap");
        assert!(
            body.lines().last().unwrap().starts_with("- "),
            "the cut backs up to a whole line, not a partial entity: {:?}",
            body.lines().last()
        );
    }
}
