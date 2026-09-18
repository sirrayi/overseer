//! Retrieval front-end patterns ported from the search/scrape batch (arsenal B2).
//!
//! Three ports in one ladder — the tiers a retrieval tool climbs when a page
//! is not plain HTML (trafilatura's `extract`, the jina reader prefix, a
//! headless-browser sidecar). All three are *patterns*: pure functions over
//! bytes plus validated data shapes. No server, no runtime, no network, no
//! browser in this crate.
//!
//! - **tier-1 local ([`Tier::Local`]) — trafilatura's `extract`**:
//!   [`extract_main`] is a hand-rolled tolerant scanner. The crate has no HTML
//!   parser and will not grow one, so this is the deliberate floor: it picks
//!   the main content region (`<article>` → `<main>` → `<body>` → whole
//!   document), drops the *content* of `script`/`style`/`noscript`/`template`/
//!   `svg`/`iframe`/`nav`/`header`/`footer`/`aside`/`form` by tracking a skip
//!   depth (a naive `<[^>]*>` strip ends the skip on any `</p>` inside a
//!   script string), decodes entities, and reports what it kept, what it
//!   dropped, and why it degraded.
//! - **tier-1.5 reader ([`Tier::Reader`]) — jina's reader prefix**:
//!   [`reader_url`] builds the render-prefix route and is idempotent, because
//!   double-wrapping a reader URL is a real failure mode that still returns
//!   200.
//! - **tier-2 browser ([`Tier::Browser`]) — a playwright sidecar**: [`Sidecar`]
//!   plus [`parse_sidecar_response`] are the helper protocol — one JSON
//!   request on stdin, one JSON response on stdout, `{"ok":true,"html":"…"}`
//!   or `{"ok":false,"error":"…"}`. The engine names the binary and validates
//!   its answer; it never spawns it here.
//!
//! [`choose`] is the single place that decides which tier runs: the ladder
//! never downgrades, so a page that needs rendering is an error naming
//! [`ENV_BROWSER`] rather than an empty tier-1 shell.
//!
//! `// DEFERRED(owner): the tier-2 browser itself (a playwright/chromium
//! sidecar process), any HTTP client, and any page fetch — this module owns
//! the *tier decision* and the *parsing* of what each tier returns; all I/O
//! stays with the tool layer.`

use std::collections::HashSet;
use std::path::PathBuf;

// ── the tier ladder ──────────────────────────────────────────────────────

/// Where an extraction ran — the ladder a retriever climbs, cheapest first.
///
/// `Local` is the always-present floor (in-process, no dependency); `Reader`
/// is the render-prefix fallback (a network fetch); `Browser` is the
/// operator's sidecar and is never reached unless the request demanded it
/// *and* a helper is configured (see [`choose`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Tier {
    /// In-process tolerant scan: [`extract_main`].
    Local,
    /// Render-prefix fetch: [`reader_url`] — tier 1.5.
    Reader,
    /// The operator's browser sidecar — tier 2, opt-in.
    Browser,
}

impl Tier {
    /// Every tier, cheapest first. This order *is* the ladder — [`Tier::rank`]
    /// is derived from it, so a reporting path and a decision path cannot
    /// disagree about which tier is weaker.
    pub const ORDER: [Tier; 3] = [Tier::Local, Tier::Reader, Tier::Browser];

    /// Stable lowercase name: the word that appears in tool output, in
    /// `Extract::note`, and in error messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Local => "local",
            Tier::Reader => "reader",
            Tier::Browser => "browser",
        }
    }

    /// Position in [`Tier::ORDER`], 0 = cheapest. The enum is closed, so the
    /// fallback is unreachable; it ranks past every real tier rather than
    /// silently claiming the cheapest slot.
    pub fn rank(self) -> usize {
        Self::ORDER
            .iter()
            .position(|t| *t == self)
            .unwrap_or(Self::ORDER.len())
    }
}

// ── tier 1: trafilatura-style main-content extraction ────────────────────

/// The main-content extraction result: what survived, what it cost, and why
/// the tier degraded (when it did).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extract {
    /// `<title>` text, whitespace-collapsed and entity-decoded; `None` when
    /// the document has no non-empty title.
    pub title: Option<String>,
    /// The extracted main text: entities decoded, whitespace runs and blank
    /// lines collapsed, chrome removed.
    pub text: String,
    /// `href` values of `<a>` elements — as written (never resolved), entity
    /// decoded, deduped first-wins, in document order.
    pub links: Vec<String>,
    /// `text.chars().count()` — characters, never bytes, so a budget derived
    /// from it can never split a multi-byte char.
    pub chars: usize,
    /// Characters of the input that did not survive (markup, chrome, skipped
    /// element content). Always `html_chars - chars`, saturating, so it can
    /// never go negative on a pathological document.
    pub dropped_chars: usize,
    /// Why the extraction degraded — the region fallback that was used, never
    /// a silent one. `None` when `<article>` supplied the text (the intended
    /// path) or when the input was empty.
    pub note: Option<String>,
}

/// Candidate content regions, best first: `(element, note label)`.
const REGIONS: [(&str, &str); 3] = [
    ("article", "<article>"),
    ("main", "<main>"),
    ("body", "<body>"),
];

/// Element *content* this scanner never keeps: script/style payloads and page
/// chrome. Dropping the content (not merely the tags) is the point — a script
/// body would otherwise read as prose, and a nav would read as the article.
const SKIPPED: [&str; 11] = [
    "script", "style", "noscript", "template", "svg", "iframe", "nav", "header", "footer", "aside",
    "form",
];

/// Elements that force a line break in the output. Inline elements
/// (`a`, `span`, `b`, `em`, `code`, …) do not, so a link inside a sentence
/// does not split it.
const BLOCKS: [&str; 34] = [
    "address",
    "article",
    "aside",
    "blockquote",
    "br",
    "dd",
    "details",
    "div",
    "dl",
    "dt",
    "fieldset",
    "figcaption",
    "figure",
    "footer",
    "form",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "header",
    "hr",
    "li",
    "main",
    "nav",
    "ol",
    "p",
    "pre",
    "section",
    "summary",
    "table",
    "tr",
    "ul",
];

/// Tier-1 extraction: pull the main content out of `html`.
///
/// Tolerant by construction: the scanner is iterative (no recursion, so
/// hostile nesting cannot blow the stack), and it never panics on unbalanced,
/// truncated, or malformed markup — it keeps the text it did read and stops.
///
/// Invariants (asserted by the tests):
/// - `chars == text.chars().count()` and `dropped_chars ==
///   html.chars().count() - chars`, both counted in *chars*;
/// - `note.is_some()` iff a fallback region was used (`<article>` is the only
///   non-degraded path), so a caller can log degradation without re-deriving
///   it;
/// - the content of every element in `SKIPPED` never appears in `text`;
/// - `links` are deduped first-wins in document order, never resolved.
pub fn extract_main(html: &str) -> Extract {
    let mut out = if html.trim().is_empty() {
        // Nothing to fall back *from*: an empty document has no degradation
        // to report, and inventing a note here would cry wolf.
        Extract {
            title: None,
            text: String::new(),
            links: Vec::new(),
            chars: 0,
            dropped_chars: 0,
            note: None,
        }
    } else {
        scan_regions(html)
    };
    out.title = title_of(html);
    out.chars = out.text.chars().count();
    out.dropped_chars = html.chars().count().saturating_sub(out.chars);
    out
}

/// Pick the best non-empty region and scan it, recording why the choice was
/// not the intended one (`<article>`).
fn scan_regions(html: &str) -> Extract {
    let mut absent: Vec<&str> = Vec::new();
    let mut empty: Vec<&str> = Vec::new();
    for (i, (tag, label)) in REGIONS.iter().enumerate() {
        let Some((start, end)) = element_inner_span(html, tag) else {
            absent.push(label);
            continue;
        };
        let (text, links) = render(&html[start..end]);
        if text.is_empty() {
            // A declared-but-empty region is not content: fall through, but
            // say so rather than reporting a bare fallback.
            empty.push(label);
            continue;
        }
        let note = (i > 0).then(|| format!("{}: used {label}", shortfall(&absent, &empty)));
        return Extract {
            title: None,
            text,
            links,
            chars: 0,
            dropped_chars: 0,
            note,
        };
    }
    let (text, links) = render(html);
    Extract {
        title: None,
        text,
        links,
        chars: 0,
        dropped_chars: 0,
        note: Some(format!(
            "{}: used the whole document",
            shortfall(&absent, &empty)
        )),
    }
}

/// The reason phrase for a fallback: what was missing and what was empty,
/// joined so the common case reads `no <article>/<main>: used <body>`.
fn shortfall(absent: &[&str], empty: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    if !absent.is_empty() {
        parts.push(format!("no {}", absent.join("/")));
    }
    if !empty.is_empty() {
        parts.push(format!("empty {}", empty.join("/")));
    }
    parts.join(": ")
}

/// The `<title>` text of `html`, collapsed to one line. An `<svg><title>` is
/// chrome (the SVG element is skipped), so it is not mistaken for the
/// document title.
fn title_of(html: &str) -> Option<String> {
    let (start, end) = element_inner_span(html, "title")?;
    let (text, _) = render(&html[start..end]);
    let line = one_line(&text);
    (!line.is_empty()).then_some(line)
}

/// Collapse every whitespace run to one space and trim — the single-line form
/// used for titles.
fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for word in s.split_whitespace() {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(word);
    }
    out
}

/// Scan a region's inner HTML (or the whole document): keep text, collect
/// links, drop the content of skipped elements.
fn render(fragment: &str) -> (String, Vec<String>) {
    let mut text = Text::default();
    let mut links: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // The element whose content is being dropped, plus how many of its own
    // open tags are still unmatched — the depth a naive strip loses.
    let mut skip: Option<(&str, usize)> = None;
    let mut pos = 0usize;
    while pos < fragment.len() {
        let Some((lt, end, body)) = next_tag(fragment, pos) else {
            if skip.is_none() {
                text.push_html(&fragment[pos..]);
            }
            break;
        };
        // Text belongs to the element it sits in: inside a skipped element it
        // is dropped, and a tag that *opens* a skip does not retroactively
        // claim the text before it.
        let kept = skip.is_none();
        if kept {
            text.push_html(&fragment[pos..lt]);
        }
        let tag = body.trim();
        if !tag.is_empty() && !tag.starts_with('!') && !tag.starts_with('?') {
            let closing = tag.starts_with('/');
            let rest = if closing { tag[1..].trim_start() } else { tag };
            let name = tag_name(rest);
            let self_closing = !closing && tag.ends_with('/');
            if let Some((open, depth)) = skip {
                // Inside a skipped element every tag is inert except the
                // matching one: `</p>` in a script string must not end it.
                if closing && name.eq_ignore_ascii_case(open) {
                    skip = if depth <= 1 {
                        None
                    } else {
                        Some((open, depth - 1))
                    };
                } else if !closing && !self_closing && name.eq_ignore_ascii_case(open) {
                    skip = Some((open, depth + 1));
                }
            } else if !closing && !self_closing && skip_element(name) {
                skip = Some((name, 1));
            } else {
                if is_block(name) {
                    text.break_block();
                }
                if !closing && name.eq_ignore_ascii_case("a") {
                    if let Some(raw) = attr(body, "href") {
                        let href = decode_entities(raw).trim().to_string();
                        if !href.is_empty() && seen.insert(href.clone()) {
                            links.push(href);
                        }
                    }
                }
            }
        }
        pos = end;
    }
    (text.finish(), links)
}

/// The inner content span `(start, end)` of the first element named `tag`,
/// skipping the content of `SKIPPED` elements so an `<article>` inside a
/// script string or a `<template>` is not mistaken for content, and so a
/// `</article>` inside a script string does not close it.
///
/// An unterminated element (truncated document) yields its content to the end
/// of input rather than nothing.
fn element_inner_span(html: &str, tag: &str) -> Option<(usize, usize)> {
    let mut pos = 0usize;
    let mut start = 0usize;
    let mut depth = 0usize;
    let mut skip: Option<(&str, usize)> = None;
    while pos < html.len() {
        let Some((lt, end, body)) = next_tag(html, pos) else {
            break;
        };
        let raw = body.trim();
        if !raw.is_empty() && !raw.starts_with('!') && !raw.starts_with('?') {
            let closing = raw.starts_with('/');
            let rest = if closing { raw[1..].trim_start() } else { raw };
            let name = tag_name(rest);
            let self_closing = !closing && raw.ends_with('/');
            if let Some((open, d)) = skip {
                if closing && name.eq_ignore_ascii_case(open) {
                    skip = if d <= 1 { None } else { Some((open, d - 1)) };
                } else if !closing && !self_closing && name.eq_ignore_ascii_case(open) {
                    skip = Some((open, d + 1));
                }
            } else if !closing && !self_closing && skip_element(name) {
                skip = Some((name, 1));
            } else if name.eq_ignore_ascii_case(tag) {
                if closing {
                    if depth > 0 {
                        depth -= 1;
                        if depth == 0 {
                            return Some((start, lt));
                        }
                    }
                } else if !self_closing {
                    if depth == 0 {
                        start = end;
                    }
                    depth += 1;
                }
            }
        }
        pos = end;
    }
    (depth > 0).then_some((start, html.len()))
}

/// Whitespace normalizer for extracted text: a run of whitespace becomes one
/// space, a run containing a line break (or a block boundary) becomes one
/// newline, and leading/trailing whitespace is dropped — so blank lines
/// collapse and no line is padded.
#[derive(Default)]
struct Text {
    out: String,
    /// Pending separator: `Some(true)` newline, `Some(false)` space.
    pending: Option<bool>,
}

impl Text {
    /// Feed raw character data with entities decoded first (a link-free, fast
    /// path when the slice carries no `&`).
    fn push_html(&mut self, s: &str) {
        if s.contains('&') {
            self.push_raw(&decode_entities(s));
        } else {
            self.push_raw(s);
        }
    }

    /// Feed raw character data.
    fn push_raw(&mut self, s: &str) {
        for ch in s.chars() {
            if ch.is_whitespace() {
                let line_break = ch == '\n' || ch == '\r';
                self.pending = Some(self.pending == Some(true) || line_break);
            } else {
                self.flush();
                self.out.push(ch);
            }
        }
    }

    /// A block-level boundary: the next text starts a new line.
    fn break_block(&mut self) {
        self.pending = Some(true);
    }

    fn flush(&mut self) {
        match self.pending.take() {
            Some(true) if !self.out.is_empty() && !self.out.ends_with('\n') => self.out.push('\n'),
            Some(false) if !self.out.is_empty() && !self.out.ends_with('\n') => self.out.push(' '),
            _ => {}
        }
    }

    fn finish(mut self) -> String {
        self.pending = None; // trailing whitespace is dropped, never padded
        self.out
    }
}

/// The next tag at or after `from`: `(offset of '<', offset just past '>',
/// body between the angle brackets)`.
///
/// Tolerant on purpose:
/// - comments are skipped as a unit (a `>` inside a comment is not a tag end);
/// - quotes inside a tag protect `>` (`<a title="a>b">` stays one tag);
/// - a `<` that cannot start a tag is left as text (`a < b`);
/// - an unterminated tag or comment is reported as a tag running to the end
///   of input, so no partial markup leaks into the extracted text.
///
/// `None` means only text remains.
fn next_tag(html: &str, from: usize) -> Option<(usize, usize, &str)> {
    let bytes = html.as_bytes();
    let mut i = from;
    while i < bytes.len() {
        let lt = i + html[i..].find('<')?;
        if bytes[lt..].starts_with(b"<!--") {
            match html[lt + 4..].find("-->") {
                Some(k) => {
                    i = lt + 4 + k + 3;
                    continue;
                }
                None => return Some((lt, bytes.len(), "")),
            }
        }
        match bytes.get(lt + 1) {
            Some(c) if c.is_ascii_alphabetic() || *c == b'/' || *c == b'!' || *c == b'?' => {}
            _ => {
                i = lt + 1; // `<` as text
                continue;
            }
        }
        let mut j = lt + 1;
        let mut quote: Option<u8> = None;
        while j < bytes.len() {
            let c = bytes[j];
            if let Some(q) = quote {
                if c == q {
                    quote = None;
                }
            } else if c == b'"' || c == b'\'' {
                quote = Some(c);
            } else if c == b'>' {
                return Some((lt, j + 1, &html[lt + 1..j]));
            }
            j += 1;
        }
        return Some((lt, bytes.len(), &html[lt + 1..]));
    }
    None
}

/// The element name at the head of a tag body (empty for a malformed tag).
fn tag_name(rest: &str) -> &str {
    let end = rest
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == ':'))
        .map(|(i, _)| i)
        .unwrap_or(rest.len());
    &rest[..end]
}

/// Whether `name` is an element whose content is dropped.
fn skip_element(name: &str) -> bool {
    SKIPPED.iter().any(|s| name.eq_ignore_ascii_case(s))
}

/// Whether `name` forces a line break in the output.
fn is_block(name: &str) -> bool {
    BLOCKS.iter().any(|s| name.eq_ignore_ascii_case(s))
}

/// The value of attribute `want` in a tag body, or `None`.
///
/// Case-insensitive on the attribute name with word boundaries, so
/// `data-href` is not `href`; whitespace around `=` is allowed; a missing
/// closing quote (truncated markup) takes the rest of the tag rather than
/// dropping the value.
fn attr<'a>(body: &'a str, want: &str) -> Option<&'a str> {
    let bytes = body.as_bytes();
    let n = want.len();
    let mut i = 0;
    while i + n <= bytes.len() {
        // Slicing is only safe at char boundaries; the ASCII check proves
        // both ends land on one.
        let candidate = if bytes[i..i + n].iter().all(u8::is_ascii) {
            &body[i..i + n]
        } else {
            i += 1;
            continue;
        };
        let boundary_before = i == 0 || bytes[i - 1].is_ascii_whitespace();
        let boundary_after = bytes
            .get(i + n)
            .is_none_or(|c| *c == b'=' || c.is_ascii_whitespace());
        if boundary_before && boundary_after && candidate.eq_ignore_ascii_case(want) {
            let mut j = i + n;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'=') {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                if j >= bytes.len() {
                    return Some("");
                }
                let quote = bytes[j];
                if quote == b'"' || quote == b'\'' {
                    let start = j + 1;
                    let end = body[start..]
                        .find(quote as char)
                        .map(|k| start + k)
                        .unwrap_or(body.len());
                    return Some(&body[start..end]);
                }
                let end = body[j..]
                    .find(char::is_whitespace)
                    .map(|k| j + k)
                    .unwrap_or(body.len());
                return Some(&body[j..end]);
            }
        }
        i += 1;
    }
    None
}

/// Decode the entities a scraper must handle: `&amp; &lt; &gt; &quot; &#39;`
/// (and `&apos;`), `&nbsp;`, and the numeric forms `&#NN;` / `&#xHH;`.
///
/// Unknown or malformed entities are left exactly as written — inventing a
/// value (or dropping the text) would corrupt the quote a caller may re-read.
/// `&nbsp;` decodes to a plain space: the whitespace collapse below owns
/// spacing, and a U+00A0 that survived would look like a word separator while
/// comparing unequal to one.
fn decode_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        let tail = &rest[amp..];
        match entity_at(tail) {
            Some((decoded, len)) => {
                out.push(decoded);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = &tail[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The entity at the head of `s` (which starts with `&`): its character and
/// its byte length, or `None` when it is not a recognised entity.
fn entity_at(s: &str) -> Option<(char, usize)> {
    let end = s.find(';')?;
    // Bound the scan: the longest entity decoded here is well under this, and
    // an unbounded search would swallow a whole paragraph between two '&'.
    if end > 12 {
        return None;
    }
    let name = &s[1..end];
    let decoded = match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ => {
            let code =
                if let Some(hex) = name.strip_prefix("#x").or_else(|| name.strip_prefix("#X")) {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    let dec = name.strip_prefix('#')?;
                    dec.parse::<u32>().ok()?
                };
            // Surrogates and out-of-range codes are not characters: leave the
            // entity verbatim rather than substituting U+FFFD.
            char::from_u32(code)?
        }
    };
    Some((decoded, end + 1))
}

// ── tier 1.5: the reader prefix ──────────────────────────────────────────

/// Host of the tier-1.5 reader; the route is `https://<READER_HOST>/<target>`.
pub const READER_HOST: &str = "r.jina.ai";

/// The tier-1.5 route for `target`: `https://r.jina.ai/<target>`.
///
/// Idempotent: a target that already points at the reader host is returned
/// unchanged (trimmed), because double-wrapping fetches the reader's own page
/// and still returns 200 — a silent wrong answer. Accepts only `http://` and
/// `https://` targets (scheme case-insensitive); anything else — `file://`,
/// `ftp://`, a bare path — is an `Err` naming the accepted schemes, never a
/// guess.
pub fn reader_url(target: &str) -> Result<String, String> {
    let target = target.trim();
    if scheme_of(target).is_none() {
        return Err(format!(
            "extract: reader_url accepts only http:// or https:// targets, got {target:?} — \
             file://, ftp://, bare paths and empty targets have no reader rendering"
        ));
    }
    if is_reader_url(target) {
        return Ok(target.to_string());
    }
    Ok(format!("https://{READER_HOST}/{target}"))
}

/// The scheme prefix of an http(s) URL, as written (`HTTPS://`), or `None`.
fn scheme_of(url: &str) -> Option<&str> {
    if starts_with_ci(url, "https://") {
        Some(&url[..8])
    } else if starts_with_ci(url, "http://") {
        Some(&url[..7])
    } else {
        None
    }
}

/// ASCII-case-insensitive prefix test (both slices are ASCII, so the byte
/// slice is always on a char boundary).
fn starts_with_ci(s: &str, prefix: &str) -> bool {
    s.len() >= prefix.len() && s.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
}

/// Whether `url` already points at the reader host. Userinfo and port are
/// stripped, so `https://user@r.jina.ai:443/x` is recognised too.
fn is_reader_url(url: &str) -> bool {
    let Some(scheme) = scheme_of(url) else {
        return false;
    };
    let rest = &url[scheme.len()..];
    let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..end];
    let host = authority.rsplit('@').next().unwrap_or(authority);
    let host = host.split(':').next().unwrap_or(host);
    host.eq_ignore_ascii_case(READER_HOST)
}

// ── tier 2: the browser sidecar ──────────────────────────────────────────

/// The variable naming the tier-2 helper binary. Unset, blank, or pointing at
/// something that is not a file means "no browser tier" — never "guess a
/// browser", so a stale path cannot masquerade as a working tier-2.
pub const ENV_BROWSER: &str = "OVERSEER_EXTRACT_BROWSER";

/// The operator's tier-2 helper.
///
/// Protocol (one request, one response, no framing beyond that):
/// - **request** — one JSON object on the helper's stdin, e.g.
///   `{"url":"https://…"}` (the helper documents its own fields; the engine
///   only decides *whether* to call it);
/// - **response** — one JSON object on stdout: `{"ok":true,"html":"…"}` with
///   the rendered HTML, or `{"ok":false,"error":"…"}` with the reason. See
///   [`parse_sidecar_response`] for what counts as valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sidecar {
    /// Path to the helper binary, as configured (symlinks are fine: existence
    /// is checked, not canonicalization).
    pub program: PathBuf,
}

impl Sidecar {
    /// The configured sidecar, or `None` when [`ENV_BROWSER`] is unset, blank,
    /// or does not name a file.
    pub fn from_env() -> Option<Sidecar> {
        Self::from_value(std::env::var(ENV_BROWSER).ok().as_deref())
    }

    /// The same rules from an explicit value, so config loading and tests do
    /// not have to touch the process environment.
    pub fn from_value(v: Option<&str>) -> Option<Sidecar> {
        let raw = v?.trim();
        if raw.is_empty() {
            return None;
        }
        let program = PathBuf::from(raw);
        if !program.is_file() {
            return None;
        }
        Some(Sidecar { program })
    }
}

/// Validate one sidecar response: `{"ok":true,"html":"…"}` yields the html,
/// `{"ok":false,"error":"…"}` its error text.
///
/// Rejects, each naming the offending field or the parse failure:
/// unparseable JSON, a missing or non-boolean `ok`, `ok:true` with a missing
/// or blank `html`, and `ok:false` with no `error`. Helper chatter on stdout
/// is tolerated: the whole output is tried first, then the last line carrying
/// a boolean `ok` object, so a helper that prints a banner still answers.
pub fn parse_sidecar_response(stdout: &str) -> Result<String, String> {
    let trimmed = stdout.trim();
    let Some(value) = sidecar_json(trimmed) else {
        return Err(format!(
            "extract: sidecar produced no JSON object with an \"ok\" field on stdout: {:?}; \
             expected {{\"ok\":true,\"html\":\"…\"}} or {{\"ok\":false,\"error\":\"…\"}}",
            truncate_chars(trimmed, 200)
        ));
    };
    match value.get("ok").and_then(|v| v.as_bool()) {
        Some(true) => match value.get("html").and_then(|v| v.as_str()) {
            Some(html) if !html.trim().is_empty() => Ok(html.to_string()),
            _ => Err(
                "extract: sidecar response is missing a non-empty \"html\" field (ok:true \
                 requires the rendered HTML)"
                    .into(),
            ),
        },
        Some(false) => Err(match value.get("error").and_then(|v| v.as_str()) {
            Some(e) if !e.trim().is_empty() => format!("extract: sidecar failed: {e}"),
            _ => "extract: sidecar failed (ok:false) and its response carries no \"error\" \
                  field naming the reason"
                .into(),
        }),
        None => Err(
            "extract: sidecar response has no boolean \"ok\" field; expected \
             {\"ok\":true,\"html\":\"…\"} or {\"ok\":false,\"error\":\"…\"}"
                .into(),
        ),
    }
}

/// The response object: the whole output when it parses, else the last line
/// that carries an `"ok"` key (a helper may log to stdout).
fn sidecar_json(stdout: &str) -> Option<serde_json::Value> {
    let whole = serde_json::from_str::<serde_json::Value>(stdout)
        .ok()
        .filter(is_response);
    whole.or_else(|| {
        stdout
            .lines()
            .rev()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok())
            .find(is_response)
    })
}

/// Whether a JSON value is a sidecar response shape (an object with `ok`).
fn is_response(v: &serde_json::Value) -> bool {
    v.is_object() && v.get("ok").is_some()
}

/// First `n` characters of `s`, with an explicit elision marker. Char-based:
/// a byte slice could split a multi-byte char and panic.
fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

// ── the decision ─────────────────────────────────────────────────────────

/// Which tiers the operator has enabled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tiers {
    /// The built-in scanner. On in [`Tiers::default`]: it costs nothing and
    /// cannot be unavailable, so "nothing configured" still extracts. Turning
    /// it off is a deliberate kill switch.
    pub local: bool,
    /// The `r.jina.ai` fallback — a network fetch, so it stays off by default.
    pub reader: bool,
    /// The tier-2 helper, `None` while unconfigured.
    pub browser: Option<Sidecar>,
}

impl Default for Tiers {
    /// Nothing configured: the local scanner, no fallback.
    fn default() -> Self {
        Tiers {
            local: true,
            reader: false,
            browser: None,
        }
    }
}

/// The tier that will serve `need`, or the reason it cannot.
///
/// The ladder never downgrades: a `Browser` need is a *demand* — a page that
/// needs rendering cannot be served by the local scanner, so answering with
/// tier-1 would return chrome or an empty shell and look like success. It
/// therefore errors naming [`ENV_BROWSER`] when no sidecar is configured.
/// `Reader` and `Local` likewise answer only from their own tier, each
/// naming the flag that would enable it.
pub fn choose(need: Tier, tiers: &Tiers) -> Result<Tier, String> {
    match need {
        Tier::Local if !tiers.local => Err(
            "extract: local extraction is disabled (Tiers.local = false); enable it or ask for a \
             higher tier"
                .into(),
        ),
        Tier::Reader if !tiers.reader => Err(format!(
            "extract: reader fallback is not enabled (Tiers.reader = false); enable it to allow \
             {READER_HOST} fetches"
        )),
        Tier::Browser if tiers.browser.is_none() => Err(format!(
            "extract: browser extraction (tier 2) needs a sidecar and none is configured: set \
             {ENV_BROWSER} to the helper binary path"
        )),
        _ => Ok(need),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A page with chrome around an article: the article must win, and the
    /// chrome must not survive.
    const PAGE: &str = r#"<!doctype html>
<html><head><title>Docs &#8212; Guide</title></head>
<body>
<nav><a href="/nav">Nav link</a></nav>
<header>Site header</header>
<article>
  <h1>Real title</h1>
  <p>The article body &amp; more.</p>
  <p>Second paragraph.</p>
  <a href="/one">One</a><a href="/one">One again</a><a href="/two?a=1&amp;b=2">Two</a>
</article>
<aside>Related links</aside>
<footer>Footer text</footer>
<p>Body-only tail text.</p>
</body></html>"#;

    /// A fresh file path under the per-process temp dir (nanos suffix so two
    /// tests in the same process cannot collide).
    fn tmp_file(tag: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("p8-extract-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{tag}-{nanos}"));
        std::fs::write(&path, b"#!/bin/sh\nexit 0\n").unwrap();
        path
    }

    #[test]
    fn article_is_preferred_over_body_and_chrome_is_dropped() {
        let e = extract_main(PAGE);
        assert!(e.text.contains("Real title"), "{}", e.text);
        assert!(e.text.contains("The article body & more."), "{}", e.text);
        assert!(e.text.contains("Second paragraph."), "{}", e.text);
        for chrome in [
            "Nav link",
            "Site header",
            "Related links",
            "Footer text",
            "Body-only tail text.",
        ] {
            assert!(
                !e.text.contains(chrome),
                "{chrome:?} survived the article preference: {}",
                e.text
            );
        }
        assert_eq!(e.title.as_deref(), Some("Docs — Guide"));
        assert_eq!(e.note, None, "the intended path is not a degradation");
    }

    #[test]
    fn script_content_never_reaches_the_text_even_with_markup_lookalikes() {
        let html = r#"<article><p>keep</p><script>var s = "</p></article></body>"; alert("boom");</script><p>also keep</p></article>"#;
        let e = extract_main(html);
        assert!(e.text.contains("keep"), "{}", e.text);
        assert!(e.text.contains("also keep"), "{}", e.text);
        for leaked in ["boom", "alert", "var s", "</p>"] {
            assert!(!e.text.contains(leaked), "{leaked:?} leaked: {}", e.text);
        }
    }

    #[test]
    fn every_skipped_element_drops_its_content() {
        for tag in SKIPPED {
            let html = format!(
                "<article><p>kept</p><{tag}>HIDDEN-{tag}<p>inner</p></{tag}><p>tail</p></article>"
            );
            let e = extract_main(&html);
            assert!(
                !e.text.contains("HIDDEN") && !e.text.contains("inner"),
                "<{tag}> content leaked: {}",
                e.text
            );
            assert!(
                e.text.contains("kept") && e.text.contains("tail"),
                "<{tag}> ate the surrounding text: {}",
                e.text
            );
        }
        // Nested same-name elements need both closes: one `</noscript>` must
        // not resume the text.
        let nested =
            "<article><noscript>HIDDEN<noscript>ALSO-HIDDEN</noscript>STILL-HIDDEN</noscript>visible</article>";
        let e = extract_main(nested);
        assert!(e.text.contains("visible"), "{}", e.text);
        assert!(
            !e.text.contains("HIDDEN") && !e.text.contains("STILL-HIDDEN"),
            "{}",
            e.text
        );
    }

    #[test]
    fn entities_decode_in_named_and_numeric_forms() {
        let html = "<article><p>a &amp; b &lt;c&gt; &quot;d&quot; &#39;e&#39; &#x27;f&#x27; \
                    &nbsp;g &#8212; h &apos;i&apos; &unknown; &amp</p></article>";
        let e = extract_main(html);
        assert!(
            e.text.contains("a & b <c> \"d\" 'e' 'f' g — h 'i'"),
            "{}",
            e.text
        );
        assert!(
            e.text.contains("&unknown;"),
            "unknown entities stay verbatim: {}",
            e.text
        );
        assert!(
            e.text.contains("&amp"),
            "an unterminated entity stays verbatim: {}",
            e.text
        );
        assert!(
            !e.text.contains("&#55357;"),
            "a surrogate code point is left verbatim, not substituted: {}",
            e.text
        );
    }

    #[test]
    fn title_is_single_line_and_only_from_the_document_title_element() {
        let e = extract_main(
            "<html><head><title>\n  Spaced   &amp;  Titled\n</title></head><body><p>x</p></body></html>",
        );
        assert_eq!(e.title.as_deref(), Some("Spaced & Titled"));
        assert!(extract_main("<body><p>no title here</p></body>")
            .title
            .is_none());
        // An SVG <title> is chrome, not the document title.
        assert!(
            extract_main("<article><svg><title>icon</title></svg><p>x</p></article>")
                .title
                .is_none()
        );
    }

    #[test]
    fn links_dedupe_in_document_order_and_are_entity_decoded() {
        let html = r#"<article>
      <a href="/b">b</a>
      <a href='/a'>a</a>
      <a href="/b">b again</a>
      <a href="/c?x=1&amp;y=2">c</a>
      <a>no href</a>
      <a href="">empty</a>
      <a href="/c?x=1&amp;y=2">c again</a>
    </article>"#;
        let e = extract_main(html);
        assert_eq!(e.links, ["/b", "/a", "/c?x=1&y=2"]);
        assert!(
            e.text.contains("b again") && e.text.contains("no href"),
            "{}",
            e.text
        );

        let odd = extract_main(
            r#"<article><a HREF="/up">U</a><a data-href="/not-a-link">D</a><a href = "/spaced" >S</a></article>"#,
        );
        assert_eq!(odd.links, ["/up", "/spaced"]);
    }

    #[test]
    fn whitespace_runs_and_blank_lines_collapse() {
        let html = "<article>\n\n  <p>alpha   beta\n\t gamma</p>\n\n\n <p>delta</p>\n</article>\n";
        let e = extract_main(html);
        assert_eq!(e.text, "alpha beta\ngamma\ndelta", "{:?}", e.text);
        assert!(
            !e.text.contains("  ") && !e.text.contains("\n\n"),
            "{:?}",
            e.text
        );
    }

    #[test]
    fn malformed_and_truncated_markup_never_panics_and_still_yields_text() {
        let cases = [
            "<article><p>hi",
            "<p>unclosed",
            "</p></div>",
            "<article><p>text</article>",
            "<a href=\"/x",
            "<script>alert(1)",
            "<article><p>keep</p><!-- unterminated comment",
            "<article><p>a < b</p></article>",
            "<ARTICLE><P>upper</P></ARTICLE>",
            "<article><p>emoji 🎉 &#55357;</p></article>",
            "<article><p>a\u{200b}b",
            "<>",
            "<!-- only a comment -->",
            "plain text, no markup at all",
        ];
        for html in cases {
            let e = extract_main(html);
            assert_eq!(e.chars, e.text.chars().count(), "{html:?}");
            assert_eq!(
                e.dropped_chars + e.chars,
                html.chars().count(),
                "{html:?}: the char budget must add up"
            );
        }
        assert!(extract_main("<article><p>hi").text.contains("hi"));
        assert!(extract_main("<ARTICLE><P>upper</P></ARTICLE>")
            .text
            .contains("upper"));
        assert!(extract_main("<article><p>a < b</p></article>")
            .text
            .contains("a < b"));
        assert!(extract_main("plain text, no markup at all")
            .text
            .contains("plain"));
    }

    #[test]
    fn deep_nesting_is_iterative_not_recursive() {
        let html = format!("<article>{}<p>deep</p></article>", "<div>".repeat(20_000));
        let e = extract_main(&html);
        assert!(e.text.contains("deep"), "chars={}", e.chars);
    }

    #[test]
    fn char_counts_are_chars_not_bytes() {
        let html = "<article><p>é🎉</p><script>var s = 'x';</script></article>";
        let e = extract_main(html);
        assert_eq!(e.text, "é🎉");
        assert_eq!(e.chars, 2);
        assert_eq!(e.dropped_chars + e.chars, html.chars().count());
    }

    #[test]
    fn note_marks_only_a_region_fallback() {
        let article = extract_main(
            "<html><body><nav>n</nav><article><p>real</p></article><p>tail</p></body></html>",
        );
        assert_eq!(article.note, None);
        assert!(article.text.contains("real") && !article.text.contains("tail"));

        let main = extract_main("<html><body><main><p>m</p></main></body></html>");
        assert_eq!(main.note.as_deref(), Some("no <article>: used <main>"));

        let body = extract_main("<html><body><p>b</p></body></html>");
        assert_eq!(
            body.note.as_deref(),
            Some("no <article>/<main>: used <body>")
        );
        assert!(body.text.contains('b'));

        let bare = extract_main("<p>fragment</p>");
        assert_eq!(
            bare.note.as_deref(),
            Some("no <article>/<main>/<body>: used the whole document")
        );
        assert!(bare.text.contains("fragment"));

        // A declared-but-empty region is named, not silently skipped.
        let empty_article = extract_main("<article></article><main><p>m</p></main>");
        assert_eq!(
            empty_article.note.as_deref(),
            Some("empty <article>: used <main>")
        );
        assert!(empty_article.text.contains('m'));

        let blank = extract_main("   ");
        assert_eq!(blank.note, None, "an empty input is not a fallback");
        assert_eq!((blank.chars, blank.text.as_str()), (0, ""));
    }

    #[test]
    fn reader_url_accepts_only_http_and_https() {
        assert_eq!(
            reader_url("https://example.com/a?b=1").unwrap(),
            "https://r.jina.ai/https://example.com/a?b=1"
        );
        assert_eq!(
            reader_url("http://example.com").unwrap(),
            "https://r.jina.ai/http://example.com"
        );
        assert_eq!(
            reader_url("HTTPS://Example.com/x").unwrap(),
            "https://r.jina.ai/HTTPS://Example.com/x"
        );
        assert_eq!(
            reader_url("  https://example.com/spaced  ").unwrap(),
            "https://r.jina.ai/https://example.com/spaced"
        );
        for bad in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "/tmp/page.html",
            "example.com",
            "",
        ] {
            let err = reader_url(bad).unwrap_err();
            assert!(
                err.contains("http://") && err.contains("https://"),
                "{bad:?} → {err}"
            );
        }
    }

    #[test]
    fn reader_url_is_idempotent_on_a_reader_url() {
        for already in [
            "https://r.jina.ai/https://example.com/a",
            "https://r.jina.ai",
            "http://r.jina.ai/x",
            "https://user@r.jina.ai:443/x",
            "https://R.JINA.AI/x",
        ] {
            assert_eq!(reader_url(already).unwrap(), already, "{already}");
        }
        // A host that merely mentions the reader is still wrapped.
        assert!(reader_url("https://example.com/r.jina.ai/x")
            .unwrap()
            .starts_with("https://r.jina.ai/https://example.com/"));
    }

    #[test]
    fn sidecar_requires_a_real_file_and_follows_the_env_var() {
        assert!(Sidecar::from_value(None).is_none());
        assert!(Sidecar::from_value(Some("")).is_none());
        assert!(Sidecar::from_value(Some("   ")).is_none());
        assert!(Sidecar::from_value(Some("/nonexistent/p8-extract-helper")).is_none());
        assert!(
            Sidecar::from_value(Some("/tmp")).is_none(),
            "a directory is not a helper"
        );
        let helper = tmp_file("helper");
        assert_eq!(
            Sidecar::from_value(Some(helper.to_str().unwrap()))
                .unwrap()
                .program,
            helper
        );
        let padded = format!("  {}  ", helper.display());
        assert_eq!(
            Sidecar::from_value(Some(&padded)).unwrap().program,
            helper,
            "surrounding whitespace is trimmed, not fatal"
        );

        // from_env reads the documented variable (the only test that touches it).
        std::env::remove_var(ENV_BROWSER);
        assert!(Sidecar::from_env().is_none());
        std::env::set_var(ENV_BROWSER, &helper);
        assert_eq!(Sidecar::from_env().unwrap().program, helper);
        std::env::set_var(ENV_BROWSER, "/nonexistent/p8-extract-helper");
        assert!(
            Sidecar::from_env().is_none(),
            "a stale path counts as unconfigured"
        );
        std::env::remove_var(ENV_BROWSER);
    }

    #[test]
    fn sidecar_response_ok_carries_the_html() {
        assert_eq!(
            parse_sidecar_response(r#"{"ok":true,"html":"<p>hi</p>"}"#).unwrap(),
            "<p>hi</p>"
        );
        let chatty = "sidecar: starting up\n{\"ok\":true,\"html\":\"<article>x</article>\"}\n";
        assert_eq!(
            parse_sidecar_response(chatty).unwrap(),
            "<article>x</article>"
        );
    }

    #[test]
    fn sidecar_response_failure_carries_the_sidecar_error() {
        let err =
            parse_sidecar_response(r#"{"ok":false,"error":"chromium not installed"}"#).unwrap_err();
        assert!(err.contains("chromium not installed"), "{err}");
        let err = parse_sidecar_response(r#"{"ok":false}"#).unwrap_err();
        assert!(err.contains("error"), "{err}");
    }

    #[test]
    fn sidecar_response_rejects_unparseable_and_htmlless_output() {
        for bad in [
            "",
            "not json at all",
            "<html>oops</html>",
            "{\"ok\":true",
            "42",
        ] {
            let err = parse_sidecar_response(bad).unwrap_err();
            assert!(err.contains("ok"), "{bad:?} → {err}");
        }
        for htmlless in [
            r#"{"ok":true}"#,
            r#"{"ok":true,"html":""}"#,
            r#"{"ok":true,"html":"   "}"#,
            r#"{"ok":true,"html":42}"#,
        ] {
            let err = parse_sidecar_response(htmlless).unwrap_err();
            assert!(err.contains("\"html\""), "{htmlless} → {err}");
        }
        let err = parse_sidecar_response(r#"{"html":"x"}"#).unwrap_err();
        assert!(err.contains("ok"), "{err}");
    }

    #[test]
    fn choose_never_downgrades_a_browser_need() {
        let tiers = Tiers {
            local: true,
            reader: true,
            browser: None,
        };
        let err = choose(Tier::Browser, &tiers).unwrap_err();
        assert!(err.contains(ENV_BROWSER), "{err}");
        assert!(err.contains("OVERSEER_EXTRACT_BROWSER"), "{err}");
        let with = Tiers {
            browser: Some(Sidecar {
                program: tmp_file("browser"),
            }),
            ..tiers
        };
        assert_eq!(choose(Tier::Browser, &with).unwrap(), Tier::Browser);
    }

    #[test]
    fn choose_gates_each_tier_on_its_own_flag_and_defaults_to_local() {
        let nothing = Tiers::default();
        assert_eq!(choose(Tier::Local, &nothing).unwrap(), Tier::Local);
        let err = choose(Tier::Reader, &nothing).unwrap_err();
        assert!(
            err.contains("reader") && err.contains("Tiers.reader"),
            "{err}"
        );

        let reader_on = Tiers {
            reader: true,
            ..nothing.clone()
        };
        assert_eq!(choose(Tier::Reader, &reader_on).unwrap(), Tier::Reader);

        let local_off = Tiers {
            local: false,
            reader: true,
            browser: None,
        };
        let err = choose(Tier::Local, &local_off).unwrap_err();
        assert!(err.contains("local"), "{err}");
        assert_eq!(choose(Tier::Reader, &local_off).unwrap(), Tier::Reader);
    }

    #[test]
    fn tier_order_ranks_local_first_and_names_are_stable() {
        assert_eq!(Tier::ORDER, [Tier::Local, Tier::Reader, Tier::Browser]);
        assert_eq!(Tier::ORDER.map(Tier::rank), [0, 1, 2]);
        assert!(Tier::Local.rank() < Tier::Reader.rank());
        assert!(Tier::Reader.rank() < Tier::Browser.rank());
        assert_eq!(
            [
                Tier::Local.as_str(),
                Tier::Reader.as_str(),
                Tier::Browser.as_str()
            ],
            ["local", "reader", "browser"]
        );
    }
}
