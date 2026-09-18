//! Web search and web extraction patterns ported from the retrieval batch
//! (arsenal B2).
//!
//! Two ports, both pure and both offline — no HTTP client, no instance, no
//! model:
//!
//! - **SearXNG `web_search`** — build the `/search?format=json` request for
//!   an instance and normalise the reply into `SearchHit`s. SearXNG is a
//!   metasearch frontend: ranking belongs to the engines it fans out to, so
//!   what is ported is the request gate (validated parameters, clamped
//!   limit) and the reply shape.
//! - **ScrapeGraphAI `web_extract`** (scrapegraph-ai) — schema-driven
//!   extraction over already-fetched text. The port keeps the half of the
//!   pattern that makes it trustworthy and drops the half that cannot exist
//!   here: every extracted value MUST be *located* in the text (a label on a
//!   line), and a field with no located value is reported missing rather
//!   than filled in. Nothing in this module can invent a value — that is the
//!   anti-hallucination contract of the port, and it is what makes an
//!   extraction quotable.
//!
//! Determinism is the point: the URL parameter order is fixed, results keep
//! the instance's order, fields keep schema order, and a repeated label
//! resolves to its first occurrence.
//!
//! `// DEFERRED(owner): the HTTP call and the instance choice for
//! web_search, and the LLM-backed half of scrapegraph's extractor — this
//! module builds the request and grounds the extraction over located text;
//! the network and the model stay with the tool layer under tools/.`

use serde_json::Value;

// ── SearXNG web_search ───────────────────────────────────────────────────

/// Upper bound on hits one search may ask for.
///
/// A public SearXNG instance fans a query out to every engine it carries, so
/// asking it for ten thousand rows is how a caller gets rate-limited or
/// banned — and the caller cannot read them anyway. `searxng_url` clamps into
/// `1..=MAX_LIMIT` deliberately rather than passing the request through: the
/// clamp is the gate's policy, not a silent truncation of results.
pub const MAX_LIMIT: usize = 50;

/// SearXNG's stock category vocabulary — the set this gate accepts.
///
/// Closed on purpose: SearXNG ignores a category it does not know, so a
/// typo'd category silently searches `general` and looks like it worked.
pub const CATEGORIES: [&str; 10] = [
    "general",
    "images",
    "videos",
    "news",
    "map",
    "music",
    "it",
    "science",
    "files",
    "social media",
];

/// The `time_range` values SearXNG understands.
pub const TIME_RANGES: [&str; 4] = ["day", "week", "month", "year"];

/// One search, in the shape the `web_search` tool accepts.
///
/// `limit` is the caller-side cap on returned hits, not a SearXNG parameter
/// (SearXNG pages with `pageno`); it is clamped into `1..=MAX_LIMIT` by
/// [`effective_limit`] and is never applied when a reply is parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchRequest {
    pub query: String,
    pub categories: Vec<String>,
    pub language: Option<String>,
    pub safesearch: u8,
    pub time_range: Option<String>,
    pub limit: usize,
}

/// The defaults a bare search runs with: `general`, moderate safe search
/// (`1`), ten hits.
///
/// `language` and `time_range` default to `None` so the instance applies its
/// own defaults — this gate does not invent a language or a window the caller
/// never asked for.
impl Default for SearchRequest {
    fn default() -> Self {
        SearchRequest {
            query: String::new(),
            categories: vec!["general".to_string()],
            language: None,
            safesearch: 1,
            time_range: None,
            limit: 10,
        }
    }
}

/// Build the JSON search URL for `base` + `req`.
///
/// Invariants (asserted by the tests):
/// - the result is `<base>/search?…&format=json` — `format=json` is always
///   present, so a caller can never accidentally scrape an HTML page;
/// - parameters appear in a fixed order (`q`, `categories`, `language`,
///   `safesearch`, `time_range`, `format`), so two runs of the same
///   request produce byte-identical URLs and logs diff cleanly;
/// - every failure names the offending field and the values that would fix it.
///
/// `req.limit` is deliberately NOT a parameter: SearXNG has no `limit` on
/// `/search` (it pages with `pageno` and returns what the instance is
/// configured to return), so writing one would send an ignored argument and
/// claim an API that does not exist. The cap is caller-side and read through
/// [`effective_limit`] — clamped so a caller cannot ask a public instance for
/// ten thousand rows.
///
/// `base` is the instance root (`https://searx.example`, with or without a
/// trailing slash); anything already carrying a query string or fragment is
/// refused, because this function appends one and a base like
/// `…/search?q=old` would produce a URL that searches something else.
/// `language` is written only when set — `None` leaves the instance default
/// in place; the query is percent-encoded (space as `+`) while `categories`
/// are encoded per entry and joined with `,` so the separator stays literal.
pub fn searxng_url(base: &str, req: &SearchRequest) -> Result<String, String> {
    let base = base.trim();
    let is_https = base
        .get(..8)
        .is_some_and(|s| s.eq_ignore_ascii_case("https://"));
    let is_http = base
        .get(..7)
        .is_some_and(|s| s.eq_ignore_ascii_case("http://"));
    if !is_https && !is_http {
        return Err(format!(
            "base `{base}` is not an http(s) URL — pass the instance root, e.g. https://searx.example"
        ));
    }
    if base.contains('?') {
        return Err(format!(
            "base `{base}` already carries a query string (`?`) — pass the instance root; this builder appends the query"
        ));
    }
    if base.contains('#') {
        return Err(format!(
            "base `{base}` already carries a fragment (`#`) — pass the instance root; this builder appends the query"
        ));
    }
    let rest = if is_https { &base[8..] } else { &base[7..] };
    let host = rest.split(['/', '?']).next().unwrap_or("");
    if host.is_empty() {
        return Err(format!(
            "base `{base}` has no host — pass the instance root, e.g. https://searx.example"
        ));
    }
    let root = base.trim_end_matches('/');

    let query = req.query.trim();
    if query.is_empty() {
        return Err("`query` is empty after trimming — a search needs terms".to_string());
    }

    if req.categories.is_empty() {
        return Err(format!(
            "`categories` is empty — SearXNG would silently fall back to `general`; pass at least one of: {}",
            category_hint()
        ));
    }
    let mut categories: Vec<String> = Vec::with_capacity(req.categories.len());
    for raw in &req.categories {
        let name = raw.trim().to_ascii_lowercase();
        if name.is_empty() {
            return Err(format!(
                "`categories` contains an empty entry — drop it or name one of: {}",
                category_hint()
            ));
        }
        if !CATEGORIES.contains(&name.as_str()) {
            return Err(format!(
                "unknown category `{name}` — accepted: {}",
                category_hint()
            ));
        }
        categories.push(percent_encode(&name));
    }

    if req.safesearch > 2 {
        return Err(format!(
            "safesearch {} is not accepted — want one of: 0 (none), 1 (moderate), 2 (strict)",
            req.safesearch
        ));
    }

    let time_range = match &req.time_range {
        None => None,
        Some(raw) => {
            let t = raw.trim().to_ascii_lowercase();
            if !TIME_RANGES.contains(&t.as_str()) {
                return Err(format!(
                    "unknown time_range `{raw}` — accepted: {}",
                    TIME_RANGES.join(", ")
                ));
            }
            Some(t)
        }
    };

    let language = match &req.language {
        None => None,
        Some(raw) => {
            let l = raw.trim();
            if l.is_empty() {
                return Err(format!(
                    "`language` is set to `{raw}`, which is empty after trimming — drop it (the instance default then applies) or pass a code like `en`"
                ));
            }
            Some(percent_encode(l))
        }
    };

    let limit = effective_limit(req);

    let mut url = format!("{root}/search?q={}", percent_encode(query));
    url.push_str("&categories=");
    url.push_str(&categories.join(","));
    if let Some(l) = &language {
        url.push_str("&language=");
        url.push_str(l);
    }
    url.push_str(&format!("&safesearch={}", req.safesearch));
    if let Some(t) = &time_range {
        url.push_str("&time_range=");
        url.push_str(t);
    }
    url.push_str("&format=json");
    // `limit` is validated above but intentionally not serialized: see
    // `effective_limit` — SearXNG's `/search` takes no limit parameter.
    let _ = limit;
    Ok(url)
}

/// The caller-side hit cap for `req`, clamped into `1..=MAX_LIMIT`.
///
/// This is the *whole* effect of `SearchRequest::limit`: it bounds how many
/// hits the caller keeps from the reply ([`parse_searxng_json`] returns the
/// instance's own result list untouched, in the engines' order). It is not a
/// query parameter — SearXNG has no `limit` on `/search` — so it is read here
/// rather than written into the URL. A `0` limit becomes `1` (a request for
/// nothing is a caller bug, not a policy) and an absurd one becomes
/// `MAX_LIMIT` (a public instance must not be asked for ten thousand rows).
pub fn effective_limit(req: &SearchRequest) -> usize {
    req.limit.clamp(1, MAX_LIMIT)
}

fn category_hint() -> String {
    CATEGORIES.join(", ")
}

/// One search hit: what the caller can act on — a title to recognise it by, a
/// URL to open, a snippet to decide with, and the engine that supplied it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub title: String,
    pub url: String,
    pub snippet: String,
    pub engine: Option<String>,
}

/// Normalise a SearXNG `format=json` reply into hits.
///
/// Invariants (asserted by the tests):
/// - result order is the instance's order — the ranking is the engines'
///   answer and re-sorting it here would claim a ranking this module does
///   not compute;
/// - an entry with a missing or blank `url` is skipped: a hit you cannot open
///   is not a hit;
/// - `limit` is NOT applied here — the caller asked for it upstream, and
///   reapplying it would truncate a caller that legitimately changed its cap;
/// - a body that is not JSON, not an object, or missing `results` is an
///   error whose message carries the first 120 characters of the body, so an
///   operator can see whether they got an HTML error page instead of JSON.
///
/// `title`/`content` that are missing or not strings read as empty and
/// `engine` reads as `None`: dropping such a hit would throw away its url,
/// which is the one field that matters.
pub fn parse_searxng_json(body: &str) -> Result<Vec<SearchHit>, String> {
    let head = head_of(body, 120);
    let value: Value = serde_json::from_str(body)
        .map_err(|e| format!("response is not JSON ({e}) — first 120 chars: {head}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| format!("response is JSON but not an object — first 120 chars: {head}"))?;
    let results = obj
        .get("results")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("response has no `results` array — first 120 chars: {head}"))?;

    let mut hits: Vec<SearchHit> = Vec::with_capacity(results.len());
    for (i, entry) in results.iter().enumerate() {
        let entry = entry.as_object().ok_or_else(|| {
            format!("results[{i}] is not an object — a SearXNG reply carries one object per hit")
        })?;
        let url = entry
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if url.is_empty() {
            continue;
        }
        hits.push(SearchHit {
            title: field_str(entry, "title"),
            url: url.to_string(),
            snippet: field_str(entry, "content"),
            engine: entry
                .get("engine")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string),
        });
    }
    Ok(hits)
}

fn field_str(entry: &serde_json::Map<String, Value>, key: &str) -> String {
    entry
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// The first `cap` characters of `s` (characters, never bytes — a cap must
/// not split a multi-byte character in an error message).
fn head_of(s: &str, cap: usize) -> String {
    s.trim().chars().take(cap).collect()
}

/// Percent-encode `s` for a query value: space becomes `+`, the unreserved
/// set `A-Za-z0-9-_.~` is kept, and every other byte of the UTF-8 encoding
/// becomes uppercase `%XX`.
///
/// Byte-wise over UTF-8 deliberately: a multi-byte character is encoded as
/// its bytes (`é` → `%C3%A9`), which is what a server decodes, while
/// iterating characters would risk emitting a code point no query parser
/// expects. A literal `+` becomes `%2B`, so it is never read back as a space.
fn percent_encode(s: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            b' ' => out.push('+'),
            _ => {
                out.push('%');
                out.push(HEX[(b >> 4) as usize] as char);
                out.push(HEX[(b & 0x0f) as usize] as char);
            }
        }
    }
    out
}

// ── scrapegraph web_extract ──────────────────────────────────────────────

/// The kind of value a field may hold, and therefore what counts as located.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldKind {
    /// Any non-empty located text.
    Text,
    /// A finite `f64` — `1,234`, `about 3`, `50%`, `nan` and `inf` are not
    /// numbers and are rejected rather than cleaned up.
    Number,
    /// A URL: `http://…`, `https://…`, or a site-absolute `/…` path.
    Url,
    /// A list: the located text split on `,`/`;`, entries trimmed, empties
    /// dropped, stored joined with `, `.
    List,
}

/// One field the extractor is asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldSpec {
    pub name: String,
    pub kind: FieldKind,
    pub required: bool,
}

/// The shape an extraction must fill: a name for the report plus the fields,
/// in the order the report will show them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionSchema {
    pub name: String,
    pub fields: Vec<FieldSpec>,
}

/// Validate a schema before it is used to ground anything.
///
/// Refuses: an unnamed schema, an empty field list, an unnamed field, a field
/// name carrying `:` or a newline (neither can ever be located, so the field
/// would silently report missing forever), and a duplicate name — labels are
/// matched case-insensitively, so a duplicate could never win and the report
/// would show the same field twice. Each error names the index and the repair.
pub fn validate_schema(s: &ExtractionSchema) -> Result<(), String> {
    let name = s.name.trim();
    if name.is_empty() {
        return Err(
            "schema name is empty — name it after the shape it extracts, e.g. `invoice`"
                .to_string(),
        );
    }
    if s.fields.is_empty() {
        return Err(format!(
            "schema `{name}` has no fields — add at least one FieldSpec, or there is nothing to ground"
        ));
    }
    for (i, f) in s.fields.iter().enumerate() {
        let fname = f.name.trim();
        if fname.is_empty() {
            return Err(format!(
                "fields[{i}].name is empty — every field needs a name to be located by"
            ));
        }
        if fname.contains(':') {
            return Err(format!(
                "fields[{i}].name `{fname}` contains `:` — that is the label separator; rename the field without it"
            ));
        }
        if fname.contains('\n') {
            return Err(format!(
                "fields[{i}].name `{fname}` contains a newline — a label lives on one line; rename the field without it"
            ));
        }
        if s.fields[..i]
            .iter()
            .any(|prev| ci_eq(prev.name.trim(), fname))
        {
            return Err(format!(
                "fields[{i}].name `{fname}` duplicates an earlier field — labels are matched case-insensitively, so the later field could never win"
            ));
        }
    }
    Ok(())
}

/// One field of an extraction result.
///
/// `value` is the located text (never re-rendered, so a report can quote it),
/// `line` is the 1-based line the label was found on, and `kind` is echoed
/// from the schema so a result is readable without it.
///
/// `line == 0` means the label was never found. A non-zero `line` with
/// `value: None` means the label *was* found and its text was refused by
/// `kind` — the two are different repairs (wrong label vs wrong kind), so the
/// result distinguishes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldValue {
    pub name: String,
    pub value: Option<String>,
    pub line: usize,
    pub kind: FieldKind,
}

/// The result of grounding a schema in a text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extraction {
    pub schema: String,
    pub fields: Vec<FieldValue>,
    /// Required fields with no value, in schema order — the honest answer
    /// where an LLM extractor would have produced a plausible sentence.
    pub missing_required: Vec<String>,
}

/// Ground `s` in `text`: locate each field's label on a line and keep the
/// text that follows it.
///
/// Three label forms are accepted, each matched case-insensitively at the
/// start of a line after optional heading/bullet/ordinal markers:
/// `Name: value`, `**Name** value` (or `*Name*`/`__Name__`/`_Name_`), and
/// `Name — value` (em dash, en dash, or a spaced hyphen). Surrounding quotes
/// and emphasis are stripped from the value.
///
/// Invariants (asserted by the tests):
/// - a value is only ever text that was in `text` — an unlocated field has
///   `value: None` and, when required, is listed in `missing_required`; this
///   function NEVER fabricates a value, which is the point of the port;
/// - the fields come out in schema order, each carrying its schema kind, so
///   two runs over the same text produce the same report;
/// - a repeated label is not a coin flip: the FIRST occurrence wins, and a
///   later one never overrides it. If that first occurrence's value fails its
///   kind check, the field keeps `value: None` but reports the line where the
///   rejected text sits — a fall-through to a later line would hide the fact
///   that the page led with something the schema cannot accept;
/// - a located value that is empty after cleaning is not a value (`Name:` on
///   its own is the shape of an empty answer, not an answer);
/// - `text.lines()` supplies the 1-based line numbers, so `\r\n` and a
///   missing trailing newline both behave.
pub fn ground(s: &ExtractionSchema, text: &str) -> Extraction {
    let mut fields: Vec<FieldValue> = s
        .fields
        .iter()
        .map(|f| FieldValue {
            name: f.name.trim().to_string(),
            value: None,
            line: 0,
            kind: f.kind,
        })
        .collect();

    for (i, line) in text.lines().enumerate() {
        for (fi, spec) in s.fields.iter().enumerate() {
            if fields[fi].line != 0 {
                continue; // resolved (acceptably or not) by an earlier line
            }
            let raw = match locate_label(line, spec.name.trim()) {
                Some(v) => v,
                None => continue,
            };
            fields[fi].line = i + 1;
            fields[fi].value = accept(spec.kind, &raw);
        }
    }

    let mut missing_required = Vec::new();
    for (spec, fv) in s.fields.iter().zip(&fields) {
        if spec.required && fv.value.is_none() {
            missing_required.push(spec.name.trim().to_string());
        }
    }

    Extraction {
        schema: s.name.trim().to_string(),
        fields,
        missing_required,
    }
}

/// Accept `raw` as a value of `kind`, or refuse it (never repair it).
fn accept(kind: FieldKind, raw: &str) -> Option<String> {
    let v = clean_value(raw);
    if v.is_empty() {
        return None;
    }
    match kind {
        FieldKind::Text => Some(v),
        FieldKind::Number => {
            let n = v.parse::<f64>().ok()?;
            if !n.is_finite() {
                return None;
            }
            Some(v)
        }
        FieldKind::Url => {
            if v.starts_with("http://") || v.starts_with("https://") || v.starts_with('/') {
                Some(v)
            } else {
                None
            }
        }
        FieldKind::List => {
            let items: Vec<&str> = v
                .split([',', ';'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .collect();
            if items.is_empty() {
                return None;
            }
            Some(items.join(", "))
        }
    }
}

/// Locate `name` on one line and return the raw text after its label.
fn locate_label(line: &str, name: &str) -> Option<String> {
    let line = strip_markers(line);
    if line.is_empty() {
        return None;
    }
    // `Name: value`
    if let Some((head, tail)) = line.split_once(':') {
        if ci_eq(&clean_label(head), name) {
            return Some(tail.trim().to_string());
        }
    }
    // `**Name** value`
    if let Some(v) = emphasis_form(line, name) {
        return Some(v);
    }
    // `Name — value`
    if let Some((head, tail)) = split_dash(line) {
        if ci_eq(&clean_label(head), name) {
            return Some(tail.trim().to_string());
        }
    }
    None
}

/// Strip leading heading/bullet/ordinal markers so a label under `- `, `# `,
/// `> `, `1. ` or `* ` still counts as being at the start of its line.
fn strip_markers(line: &str) -> &str {
    let mut s = line.trim_start();
    loop {
        let before = s.len();
        s = s.trim_start_matches(['#', '>', '•']);
        s = s.trim_start();
        if let Some(rest) = s
            .strip_prefix('-')
            .or_else(|| s.strip_prefix('*'))
            .or_else(|| s.strip_prefix('+'))
        {
            if rest.starts_with(char::is_whitespace) {
                s = rest.trim_start();
            }
        }
        let digits = s.chars().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            if let Some(rest) = s[digits..]
                .strip_prefix('.')
                .or_else(|| s[digits..].strip_prefix(')'))
            {
                if rest.starts_with(char::is_whitespace) {
                    s = rest.trim_start();
                }
            }
        }
        if s.len() == before {
            break;
        }
    }
    s
}

/// `**Name** value` and its single-emphasis / underscore spellings.
fn emphasis_form(line: &str, name: &str) -> Option<String> {
    let lead = line.chars().next().filter(|c| *c == '*' || *c == '_')?;
    let after_open = line.trim_start_matches(lead);
    let tail = strip_prefix_ci(after_open, name)?;
    let after_close = tail.trim_start_matches(lead);
    if after_close.len() == tail.len() {
        return None; // the name was there but never closed its emphasis
    }
    Some(after_close.trim().to_string())
}

/// Split a `Name — value` line on the dash family (em dash, en dash, or a
/// hyphen with whitespace on both sides).
fn split_dash(line: &str) -> Option<(&str, &str)> {
    for (i, c) in line.char_indices() {
        match c {
            '—' | '–' => return Some((&line[..i], &line[i + c.len_utf8()..])),
            '-' if i > 0
                && line[..i].ends_with(char::is_whitespace)
                && line[i + 1..].starts_with(char::is_whitespace) =>
            {
                return Some((&line[..i], &line[i + 1..]));
            }
            _ => {}
        }
    }
    None
}

fn is_wrap(c: char) -> bool {
    matches!(c, '*' | '_' | '`' | '"' | '\'' | '“' | '”' | '‘' | '’')
}

fn close_for(open: char) -> char {
    match open {
        '“' => '”',
        '‘' => '’',
        other => other,
    }
}

/// Strip the emphasis/quote pair a value arrived wrapped in, plus stray
/// trailing emphasis. A value that merely *contains* an asterisk (`a*b`) is
/// left alone — only the wrapping is removed.
fn clean_value(raw: &str) -> String {
    let mut v = raw.trim();
    if let Some(open) = v.chars().next().filter(|c| is_wrap(*c)) {
        v = v.trim_start_matches(open);
        v = v.trim_end_matches(close_for(open));
    }
    v.trim_matches(|c| c == '*' || c == '_').trim().to_string()
}

/// Strip emphasis wrapping from a label before comparing it to a field name.
fn clean_label(raw: &str) -> String {
    raw.trim().trim_matches(is_wrap).trim().to_string()
}

/// Case-insensitive equality without allocating, trimming both sides first.
fn ci_eq(a: &str, b: &str) -> bool {
    let mut bi = b.trim().chars().flat_map(char::to_lowercase);
    for ca in a.trim().chars().flat_map(char::to_lowercase) {
        match bi.next() {
            Some(cb) if cb == ca => {}
            _ => return false,
        }
    }
    bi.next().is_none()
}

/// Case-insensitive prefix removal, returning the untouched remainder so the
/// byte offset (and therefore the value text) is exact.
fn strip_prefix_ci<'a>(s: &'a str, name: &str) -> Option<&'a str> {
    let mut idx = 0usize;
    let mut chars = s.chars();
    for nc in name.chars() {
        let sc = chars.next()?;
        let mut sl = sc.to_lowercase();
        let mut nl = nc.to_lowercase();
        loop {
            match (sl.next(), nl.next()) {
                (None, None) => break,
                (Some(x), Some(y)) if x == y => continue,
                _ => return None,
            }
        }
        idx += sc.len_utf8();
    }
    Some(&s[idx..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn req(query: &str) -> SearchRequest {
        SearchRequest {
            query: query.to_string(),
            ..Default::default()
        }
    }

    fn text_field(name: &str, required: bool) -> FieldSpec {
        FieldSpec {
            name: name.to_string(),
            kind: FieldKind::Text,
            required,
        }
    }

    // ── searxng_url ──────────────────────────────────────────────────────

    #[test]
    fn url_builds_a_plain_query_in_a_fixed_parameter_order() {
        let url = searxng_url("https://searx.example", &req("rust lang")).unwrap();
        assert_eq!(
            url,
            "https://searx.example/search?q=rust+lang&categories=general&safesearch=1&format=json"
        );
        assert!(
            !url.contains("limit"),
            "SearXNG has no `limit` on /search — the cap is caller-side: {url}"
        );
        // A trailing slash on the root is not doubled, and a sub-path install
        // keeps its prefix.
        assert_eq!(
            searxng_url("https://searx.example/", &req("x")).unwrap(),
            searxng_url("https://searx.example", &req("x")).unwrap()
        );
        assert!(searxng_url("https://host/searx/", &req("x"))
            .unwrap()
            .starts_with("https://host/searx/search?q=x"));
        // The query is trimmed, and `format=json` is never optional.
        assert!(searxng_url("http://host", &req("  x  "))
            .unwrap()
            .starts_with("http://host/search?q=x&"));
    }

    #[test]
    fn url_encodes_space_as_plus() {
        let url = searxng_url("https://searx.example", &req("two  words")).unwrap();
        assert!(
            url.starts_with("https://searx.example/search?q=two++words&"),
            "{url}"
        );
        // A literal `+` must not decode back into a space.
        let plus = searxng_url("https://searx.example", &req("c++")).unwrap();
        assert!(plus.contains("q=c%2B%2B&"), "{plus}");
    }

    #[test]
    fn url_percent_encodes_unicode_and_reserved_bytes_as_uppercase_utf8_hex() {
        let url = searxng_url("https://searx.example", &req("café & tea=1*~_")).unwrap();
        assert!(url.contains("q=caf%C3%A9+%26+tea%3D1%2A~_&"), "{url}");
        // Uppercase hex, and a 4-byte character still encodes byte-wise.
        let emoji = searxng_url("https://searx.example", &req("🚀")).unwrap();
        assert!(emoji.contains("q=%F0%9F%9A%80&"), "{emoji}");
    }

    #[test]
    fn url_refuses_a_base_that_already_carries_a_query() {
        let err = searxng_url("https://searx.example/search?q=old", &req("x")).unwrap_err();
        assert!(err.contains("already carries a query string"), "{err}");
        assert!(
            err.contains("https://searx.example/search?q=old"),
            "names the base: {err}"
        );
        let frag = searxng_url("https://searx.example/#top", &req("x")).unwrap_err();
        assert!(frag.contains("fragment"), "{frag}");
    }

    #[test]
    fn url_refuses_a_base_that_is_not_http() {
        let err = searxng_url("searx.example", &req("x")).unwrap_err();
        assert!(err.contains("not an http(s) URL"), "{err}");
        assert!(searxng_url("ftp://searx.example", &req("x"))
            .unwrap_err()
            .contains("http(s)"));
        assert!(searxng_url("https://", &req("x"))
            .unwrap_err()
            .contains("no host"));
    }

    #[test]
    fn url_refuses_an_empty_query_after_trim() {
        let err = searxng_url("https://searx.example", &req("   ")).unwrap_err();
        assert!(err.contains("`query` is empty after trimming"), "{err}");
    }

    #[test]
    fn url_refuses_safesearch_outside_the_closed_set() {
        let mut r = req("x");
        r.safesearch = 3;
        let err = searxng_url("https://searx.example", &r).unwrap_err();
        assert!(err.contains("0 (none), 1 (moderate), 2 (strict)"), "{err}");
        for ok in [0u8, 1, 2] {
            r.safesearch = ok;
            assert!(searxng_url("https://searx.example", &r)
                .unwrap()
                .contains(&format!("&safesearch={ok}&")));
        }
    }

    #[test]
    fn url_refuses_an_unknown_time_range_and_normalizes_case() {
        let mut r = req("x");
        r.time_range = Some("fortnight".to_string());
        let err = searxng_url("https://searx.example", &r).unwrap_err();
        assert!(err.contains("fortnight"), "{err}");
        assert!(err.contains("day, week, month, year"), "{err}");
        r.time_range = Some(" Week ".to_string());
        assert!(searxng_url("https://searx.example", &r)
            .unwrap()
            .contains("&time_range=week&"));
    }

    #[test]
    fn url_refuses_empty_or_unknown_categories() {
        let mut r = req("x");
        r.categories = Vec::new();
        let err = searxng_url("https://searx.example", &r).unwrap_err();
        assert!(err.contains("`categories` is empty"), "{err}");
        assert!(
            err.contains("general, images"),
            "names the accepted set: {err}"
        );
        r.categories = vec!["vibes".to_string()];
        let err = searxng_url("https://searx.example", &r).unwrap_err();
        assert!(err.contains("unknown category `vibes`"), "{err}");
        assert!(err.contains("science"), "names the accepted set: {err}");
        r.categories = vec!["  ".to_string()];
        assert!(searxng_url("https://searx.example", &r)
            .unwrap_err()
            .contains("empty entry"));
        // Two categories join with a literal comma, and `social media` keeps
        // its space as `+`.
        r.categories = vec!["NEWS".to_string(), "social media".to_string()];
        assert!(searxng_url("https://searx.example", &r)
            .unwrap()
            .contains("&categories=news,social+media&"));
    }

    #[test]
    fn the_limit_is_caller_side_clamped_and_never_a_query_parameter() {
        let mut r = req("x");
        // A request for nothing becomes one hit; an absurd request becomes
        // the public-instance budget. Both through `effective_limit`.
        r.limit = 0;
        assert_eq!(effective_limit(&r), 1);
        r.limit = 10_000;
        assert_eq!(effective_limit(&r), MAX_LIMIT);
        r.limit = MAX_LIMIT;
        assert_eq!(effective_limit(&r), MAX_LIMIT);
        r.limit = 1;
        assert_eq!(effective_limit(&r), 1);
        // …and none of it reaches the wire: SearXNG's /search takes no
        // `limit`, so writing one would send an argument the instance
        // ignores while claiming an API that does not exist.
        for limit in [0usize, 3, 10_000] {
            r.limit = limit;
            let url = searxng_url("https://searx.example", &r).unwrap();
            assert!(!url.contains("limit"), "limit={limit} leaked: {url}");
        }
    }

    #[test]
    fn url_omits_language_unless_set() {
        let mut r = req("x");
        let url = searxng_url("https://searx.example", &r).unwrap();
        assert!(
            !url.contains("language="),
            "None leaves the instance default: {url}"
        );
        r.language = Some("en-US".to_string());
        assert!(searxng_url("https://searx.example", &r)
            .unwrap()
            .contains("&language=en-US&"));
        r.language = Some("  ".to_string());
        assert!(searxng_url("https://searx.example", &r)
            .unwrap_err()
            .contains("`language` is set to"));
        // A language code with a space cannot smuggle in a second parameter.
        r.language = Some("en &format=html".to_string());
        let url = searxng_url("https://searx.example", &r).unwrap();
        assert!(url.contains("&language=en+%26format%3Dhtml&"), "{url}");
        assert_eq!(
            url.matches("&format=").count(),
            1,
            "the injected `&format=html` stays inside one encoded value: {url}"
        );
    }

    // ── parse_searxng_json ───────────────────────────────────────────────

    #[test]
    fn parse_reads_a_three_result_fixture_in_order() {
        let body = json!({
            "query": "rust",
            "number_of_results": 3,
            "results": [
                {"title": "Rust", "url": "https://rust-lang.org", "content": "systems language", "engine": "duckduckgo"},
                {"title": "Docs", "url": "https://doc.rust-lang.org", "content": "the book", "engine": "bing"},
                {"title": "Crates", "url": "https://crates.io", "content": "packages"}
            ]
        })
        .to_string();
        let hits = parse_searxng_json(&body).unwrap();
        let urls: Vec<&str> = hits.iter().map(|h| h.url.as_str()).collect();
        assert_eq!(
            urls,
            vec![
                "https://rust-lang.org",
                "https://doc.rust-lang.org",
                "https://crates.io"
            ]
        );
        assert_eq!(hits[0].title, "Rust");
        assert_eq!(hits[0].snippet, "systems language");
        assert_eq!(hits[0].engine.as_deref(), Some("duckduckgo"));
        assert_eq!(
            hits[2].engine, None,
            "a missing engine is None, not a guess"
        );
    }

    #[test]
    fn parse_skips_entries_that_carry_no_url() {
        let body = json!({
            "results": [
                {"title": "kept", "url": "https://a.example"},
                {"title": "no url at all"},
                {"title": "blank", "url": "   "},
                {"title": "padded", "url": " https://b.example "},
                {"title": "null", "url": null}
            ]
        })
        .to_string();
        let hits = parse_searxng_json(&body).unwrap();
        assert_eq!(
            hits.len(),
            2,
            "a hit you cannot open is not a hit: {hits:?}"
        );
        assert_eq!(hits[0].url, "https://a.example");
        assert_eq!(hits[1].url, "https://b.example");
        // A non-string title reads as empty rather than dropping the hit.
        let body = json!({"results": [{"title": 7, "url": "https://c.example"}]}).to_string();
        let hits = parse_searxng_json(&body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "");
    }

    #[test]
    fn parse_reports_a_non_json_body_with_its_first_120_chars() {
        let body = "<html><body><h1>Too Many Requests</h1></body></html>";
        let err = parse_searxng_json(body).unwrap_err();
        assert!(err.contains("not JSON"), "{err}");
        assert!(
            err.contains("Too Many Requests"),
            "an operator can see the page: {err}"
        );
        let long = "x".repeat(300);
        let err = parse_searxng_json(&long).unwrap_err();
        assert!(err.contains(&"x".repeat(120)), "keeps 120 chars");
        assert!(!err.contains(&"x".repeat(121)), "and no more: {err}");
    }

    #[test]
    fn parse_reports_a_body_that_is_not_a_results_object() {
        let err = parse_searxng_json("[1,2,3]").unwrap_err();
        assert!(err.contains("not an object"), "{err}");
        let err = parse_searxng_json(&json!({"error": "search disabled"}).to_string()).unwrap_err();
        assert!(err.contains("no `results` array"), "{err}");
        assert!(
            err.contains("search disabled"),
            "carries the body head: {err}"
        );
        let err = parse_searxng_json(&json!({"results": [null]}).to_string()).unwrap_err();
        assert!(err.contains("results[0] is not an object"), "{err}");
    }

    #[test]
    fn parse_keeps_every_result_because_the_cap_belongs_to_the_caller() {
        let results: Vec<Value> = (0..12)
            .map(|i| json!({"title": format!("r{i}"), "url": format!("https://h{i}.example")}))
            .collect();
        let body = json!({"results": results}).to_string();
        // The request asked for 3, and parse still returns all 12: the limit
        // is the caller's own cap (SearXNG has none on /search) and
        // re-applying it here would double-apply it.
        let request = SearchRequest {
            query: "x".to_string(),
            limit: 3,
            ..Default::default()
        };
        assert_eq!(effective_limit(&request), 3);
        assert!(!searxng_url("https://searx.example", &request)
            .unwrap()
            .contains("limit"));
        assert_eq!(parse_searxng_json(&body).unwrap().len(), 12);
    }

    // ── validate_schema ──────────────────────────────────────────────────

    #[test]
    fn schema_refuses_empty_names_duplicates_and_an_empty_field_list() {
        let ok = ExtractionSchema {
            name: "invoice".to_string(),
            fields: vec![text_field("Title", true), text_field("Vendor", false)],
        };
        assert!(validate_schema(&ok).is_ok());

        let unnamed = ExtractionSchema {
            name: "  ".to_string(),
            fields: ok.fields.clone(),
        };
        assert!(validate_schema(&unnamed)
            .unwrap_err()
            .contains("schema name is empty"));

        let empty = ExtractionSchema {
            name: "invoice".to_string(),
            fields: Vec::new(),
        };
        let err = validate_schema(&empty).unwrap_err();
        assert!(err.contains("has no fields"), "{err}");

        let nameless = ExtractionSchema {
            name: "invoice".to_string(),
            fields: vec![text_field("Title", true), text_field(" ", false)],
        };
        assert!(validate_schema(&nameless)
            .unwrap_err()
            .contains("fields[1].name is empty"));

        // Case-insensitive duplicate: the later one could never win.
        let dup = ExtractionSchema {
            name: "invoice".to_string(),
            fields: vec![text_field("Title", true), text_field("title", true)],
        };
        let err = validate_schema(&dup).unwrap_err();
        assert!(err.contains("duplicates an earlier field"), "{err}");
        assert!(err.contains("fields[1]"), "{err}");
    }

    #[test]
    fn schema_refuses_a_field_name_that_cannot_be_located() {
        let colon = ExtractionSchema {
            name: "s".to_string(),
            fields: vec![text_field("Total: gross", true)],
        };
        assert!(validate_schema(&colon)
            .unwrap_err()
            .contains("label separator"));
        let newline = ExtractionSchema {
            name: "s".to_string(),
            fields: vec![text_field("Total\ngross", true)],
        };
        assert!(validate_schema(&newline).unwrap_err().contains("newline"));
    }

    // ── ground ───────────────────────────────────────────────────────────

    fn schema(name: &str, fields: Vec<FieldSpec>) -> ExtractionSchema {
        ExtractionSchema {
            name: name.to_string(),
            fields,
        }
    }

    #[test]
    fn ground_matches_the_three_label_forms() {
        let s = schema(
            "book",
            vec![
                text_field("Title", true),
                FieldSpec {
                    name: "Author".to_string(),
                    kind: FieldKind::Text,
                    required: true,
                },
                FieldSpec {
                    name: "Year".to_string(),
                    kind: FieldKind::Number,
                    required: true,
                },
            ],
        );
        let text = "Title: Dune\n**Author** Frank Herbert\nYear — 1965";
        let out = ground(&s, text);
        assert_eq!(out.schema, "book");
        assert_eq!(out.fields[0].value.as_deref(), Some("Dune"));
        assert_eq!(out.fields[1].value.as_deref(), Some("Frank Herbert"));
        assert_eq!(out.fields[2].value.as_deref(), Some("1965"));
        assert_eq!(out.fields[0].line, 1);
        assert_eq!(out.fields[1].line, 2);
        assert_eq!(out.fields[2].line, 3);
        assert!(out.missing_required.is_empty());
        // Underline and single-emphasis spellings, and the spaced hyphen.
        let text = "__A__ one\n_B_ two\nC - three";
        let s = schema(
            "s",
            vec![
                text_field("A", true),
                text_field("B", true),
                text_field("C", true),
            ],
        );
        let out = ground(&s, text);
        assert_eq!(out.fields[0].value.as_deref(), Some("one"));
        assert_eq!(out.fields[1].value.as_deref(), Some("two"));
        assert_eq!(out.fields[2].value.as_deref(), Some("three"));
        // Case-insensitive matching, and no prefix sloppiness.
        let out = ground(&s, "alpha: nope\na: yes");
        assert_eq!(out.fields[0].value.as_deref(), Some("yes"));
    }

    #[test]
    fn ground_grounds_under_list_markers_and_headings() {
        let s = schema(
            "s",
            vec![
                text_field("Title", true),
                text_field("Vendor", true),
                text_field("Total", true),
                text_field("Note", true),
            ],
        );
        let text = "# Title: Dune\n- Vendor: Acme\n1. Total: 42\n* Note: ok";
        let out = ground(&s, text);
        assert_eq!(out.fields[0].value.as_deref(), Some("Dune"));
        assert_eq!(out.fields[1].value.as_deref(), Some("Acme"));
        assert_eq!(out.fields[2].value.as_deref(), Some("42"));
        assert_eq!(out.fields[3].value.as_deref(), Some("ok"));
        assert!(
            out.missing_required.is_empty(),
            "{:?}",
            out.missing_required
        );
    }

    #[test]
    fn ground_strips_quotes_and_emphasis_from_the_value() {
        let s = schema(
            "s",
            vec![
                text_field("A", true),
                text_field("B", true),
                text_field("C", true),
                text_field("D", true),
            ],
        );
        let text = "A: \"Dune\"\nB: `code`\nC: **bold**\nD: a*b";
        let out = ground(&s, text);
        assert_eq!(out.fields[0].value.as_deref(), Some("Dune"));
        assert_eq!(out.fields[1].value.as_deref(), Some("code"));
        assert_eq!(out.fields[2].value.as_deref(), Some("bold"));
        assert_eq!(
            out.fields[3].value.as_deref(),
            Some("a*b"),
            "inner emphasis is not wrapping"
        );
    }

    #[test]
    fn ground_rejects_values_that_do_not_fit_their_kind() {
        let s = schema(
            "s",
            vec![
                FieldSpec {
                    name: "Price".to_string(),
                    kind: FieldKind::Number,
                    required: false,
                },
                FieldSpec {
                    name: "Site".to_string(),
                    kind: FieldKind::Url,
                    required: false,
                },
            ],
        );
        for bad in ["1,234", "about 3", "50%", "nan", "inf", ""] {
            let text = format!("Price: {bad}");
            let out = ground(&s, &text);
            assert_eq!(out.fields[0].value, None, "`{bad}` is not a number");
            assert_eq!(out.fields[0].line, 1, "the rejected line is still reported");
        }
        let good = ground(&s, "Price: -3.5");
        assert_eq!(good.fields[0].value.as_deref(), Some("-3.5"));
        for bad in ["example.com/x", "www.example.com", "mailto:a@b"] {
            let out = ground(&s, &format!("Site: {bad}"));
            assert_eq!(out.fields[1].value, None, "`{bad}` is not a URL");
        }
        for good_url in ["https://x.example/a", "http://x.example", "/docs/a"] {
            let out = ground(&s, &format!("Site: {good_url}"));
            assert_eq!(out.fields[1].value.as_deref(), Some(good_url));
        }
    }

    #[test]
    fn ground_splits_lists_and_drops_empty_entries() {
        let s = schema(
            "s",
            vec![FieldSpec {
                name: "Tags".to_string(),
                kind: FieldKind::List,
                required: true,
            }],
        );
        let out = ground(&s, "Tags: alpha, beta ;; gamma");
        assert_eq!(out.fields[0].value.as_deref(), Some("alpha, beta, gamma"));
        assert_eq!(out.fields[0].kind, FieldKind::List);
        // A list that splits into nothing is not a value.
        let out = ground(&s, "Tags: , ;");
        assert_eq!(out.fields[0].value, None);
        assert_eq!(out.missing_required, vec!["Tags".to_string()]);
    }

    #[test]
    fn ground_reports_required_fields_that_are_absent_and_invents_nothing() {
        let s = schema(
            "receipt",
            vec![
                text_field("Vendor", true),
                FieldSpec {
                    name: "Price".to_string(),
                    kind: FieldKind::Number,
                    required: true,
                },
                text_field("Note", false),
            ],
        );
        let text = "Vendor: Acme\nThis page never states a price.\nNote:";
        let out = ground(&s, text);
        assert_eq!(out.fields[0].value.as_deref(), Some("Acme"));
        assert_eq!(out.fields[1].value, None, "the port never invents a value");
        assert_eq!(out.fields[1].line, 0, "no label was found at all");
        assert_eq!(out.missing_required, vec!["Price".to_string()]);
        assert_eq!(out.fields[2].value, None, "an empty value is not a value");

        // Every value that did come out is literally in the text.
        let text = "Vendor: Acme Corp\nPrice: 12.50\nNote: shipped";
        let out = ground(&s, text);
        for fv in &out.fields {
            if let Some(v) = &fv.value {
                assert!(
                    text.contains(v.as_str()),
                    "`{v}` must be located in the text"
                );
            }
        }
        assert!(out.missing_required.is_empty());
    }

    #[test]
    fn ground_keeps_the_first_occurrence_of_a_repeated_label() {
        let s = schema(
            "s",
            vec![text_field("Title", true), text_field("Title alt", false)],
        );
        let text = "junk\nTitle: first\nTitle: second";
        let out = ground(&s, text);
        assert_eq!(out.fields[0].value.as_deref(), Some("first"));
        assert_eq!(out.fields[0].line, 2);
        // A rejected first occurrence is reported, not skipped over: the
        // field stays empty and keeps the line of the value that failed.
        let s = schema(
            "s",
            vec![FieldSpec {
                name: "Price".to_string(),
                kind: FieldKind::Number,
                required: true,
            }],
        );
        let out = ground(&s, "Price: about three\nPrice: 5");
        assert_eq!(out.fields[0].value, None);
        assert_eq!(out.fields[0].line, 1);
        assert_eq!(out.missing_required, vec!["Price".to_string()]);
    }

    #[test]
    fn ground_records_one_based_line_numbers_and_schema_field_order() {
        let s = schema(
            "s",
            vec![
                text_field("First", true),
                text_field("Second", true),
                text_field("Third", true),
            ],
        );
        let text = "prose line one\n\nprose line three\nFirst: a\n\nSecond: b\nThird: c";
        let out = ground(&s, text);
        let names: Vec<&str> = out.fields.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["First", "Second", "Third"],
            "schema order, not label order"
        );
        assert_eq!(out.fields[0].line, 4);
        assert_eq!(out.fields[1].line, 6);
        assert_eq!(out.fields[2].line, 7);
        // Two runs over the same text agree exactly.
        assert_eq!(out, ground(&s, text));
        // Kind is echoed from the schema.
        let s = schema(
            "s",
            vec![FieldSpec {
                name: "First".to_string(),
                kind: FieldKind::Url,
                required: true,
            }],
        );
        let out = ground(&s, "First: /docs");
        assert_eq!(out.fields[0].kind, FieldKind::Url);
    }

    #[test]
    fn ground_handles_crlf_and_a_missing_trailing_newline() {
        let s = schema(
            "s",
            vec![text_field("Title", true), text_field("Vendor", true)],
        );
        let out = ground(&s, "Title: Dune\r\nVendor: Acme");
        assert_eq!(out.fields[0].value.as_deref(), Some("Dune"));
        assert_eq!(out.fields[1].value.as_deref(), Some("Acme"));
        assert_eq!(out.fields[1].line, 2);
    }
}
