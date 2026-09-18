//! Crawl-and-scrape patterns ported from the retrieval batch (arsenal B2).
//!
//! Three ports, all pure data-plus-decision logic — no HTTP client, no
//! browser, no server, no new dependency:
//!
//! - **crawl4ai crawl config** — the bounds a crawl config must satisfy
//!   (`validate_crawl`), the verdict every discovered URL gets (`decide`),
//!   and the deterministic tier built out of them (`frontier`).
//! - **firecrawl 4-endpoint vocabulary** — the wire shape of the four
//!   endpoints: their names and paths (`Endpoint`) and the request body each
//!   one accepts (`firecrawl_body`). The firecrawl server is AGPL-3.0, so
//!   only the *public API shape* is expressed here; no code is taken or
//!   linked.
//! - **scrapy request/response middleware chain** — the documented asymmetry
//!   (request middleware runs in declared order, response middleware in
//!   reverse, because the innermost middleware is the first to see the
//!   response) with two dependency-free built-in steps, `SetKey` and
//!   `RenameKey`, so the order is observable and testable.
//!
//! `// DEFERRED(owner): actually fetching anything — an HTTP client, a
//! headless browser, robots.txt, real rate limiting, retries, and the
//! crawl4ai/firecrawl runtimes — the ports here are the gate a fetcher calls
//! and the body it would send; network I/O stays out by scope.`

use serde_json::{Map, Value};

// ── crawl4ai crawl config ────────────────────────────────────────────────

/// Inclusive upper bound on `max_depth` — a crawl deeper than this is a
/// spend bug, not a wider net.
pub const MAX_CRAWL_DEPTH: usize = 10;

/// Inclusive upper bound on `max_pages`.
pub const MAX_CRAWL_PAGES: usize = 1000;

/// Inclusive upper bound on `concurrency` — above this, a polite crawl stops
/// being polite.
pub const MAX_CRAWL_CONCURRENCY: usize = 16;

/// One crawl's policy: how far to go, which URLs are in scope, whether to
/// stay on the site, and how hard to hit it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CrawlConfig {
    pub max_depth: usize,
    pub max_pages: usize,
    /// Globs a URL must match to be crawled (empty = every URL is in scope).
    pub include: Vec<String>,
    /// Globs that put a URL out of scope; an exclude always beats an include.
    pub exclude: Vec<String>,
    pub same_domain: bool,
    /// Pause between requests. Unbounded: operator politeness, not a bound.
    pub delay_ms: u64,
    pub concurrency: usize,
}

/// Validate a crawl config before a single page is fetched.
///
/// The boundaries are `max_depth` 0..=10, `max_pages` 1..=1000 and
/// `concurrency` 1..=16; every error names the offending field **and** its
/// accepted range, so a bad config is repaired from the message alone.
/// `delay_ms` is deliberately unbounded (see the field doc).
///
/// Every `include`/`exclude` entry must be a non-blank glob — a blank pattern
/// is a typo, and a blank entry in `include` silently widens the crawl to
/// everything. An `exclude` of `**` is refused outright: an exclude that
/// matches everything crawls nothing, so accepting it would turn a typo into
/// a crawl that discovers the whole site and fetches none of it.
pub fn validate_crawl(c: &CrawlConfig) -> Result<(), String> {
    if c.max_depth > MAX_CRAWL_DEPTH {
        return Err(format!(
            "`max_depth` must be in 0..={MAX_CRAWL_DEPTH}, got {}",
            c.max_depth
        ));
    }
    if c.max_pages == 0 || c.max_pages > MAX_CRAWL_PAGES {
        return Err(format!(
            "`max_pages` must be in 1..={MAX_CRAWL_PAGES}, got {}",
            c.max_pages
        ));
    }
    if c.concurrency == 0 || c.concurrency > MAX_CRAWL_CONCURRENCY {
        return Err(format!(
            "`concurrency` must be in 1..={MAX_CRAWL_CONCURRENCY}, got {}",
            c.concurrency
        ));
    }
    for (field, patterns) in [("include", &c.include), ("exclude", &c.exclude)] {
        for pattern in patterns {
            if pattern.trim().is_empty() {
                return Err(format!(
                    "`{field}` contains a blank glob — every pattern must be a non-empty string"
                ));
            }
        }
    }
    if c.exclude.iter().any(|p| p == "**") {
        return Err(
            "`exclude` contains `**` — an exclude that matches everything crawls nothing; drop \
             the exclude or bound the crawl with `max_pages` instead"
                .to_string(),
        );
    }
    Ok(())
}

/// What the fetcher does with one discovered URL; `Fetch` is the only action,
/// the rest report which rule refused it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Fetch,
    SkipDepth,
    SkipFilter,
    SkipDomain,
    SkipSeen,
    SkipPageCap,
}

/// Decide one URL, in a **fixed precedence** — the first rule that refuses
/// wins, so a URL failing two rules always reports the earlier one:
///
/// 1. `SkipDepth` — `depth > max_depth`.
/// 2. `SkipFilter` — an `exclude` glob matches, or `include` is non-empty and
///    no `include` glob matches. Exclude wins over include.
/// 3. `SkipDomain` — `same_domain` and the URL's host is not the root host.
/// 4. `SkipSeen` — `url` is already in `seen` (exact string match: the
///    caller's seen-set is its identity set, so a trailing slash is a
///    different page here).
/// 5. `SkipPageCap` — `fetched >= max_pages`.
///
/// Otherwise `Fetch`. A root URL at depth 0 with an empty seen-set passes all
/// five by construction (`0 <= max_depth`, `0 < max_pages`), which is why a
/// valid crawl always starts.
///
/// The same-domain rule is **exact host equality, case-insensitive**: root
/// `example.com` accepts `HTTP://Example.COM/a` and rejects `a.example.com` —
/// a subdomain is a different site, and a `.<root>` suffix rule would let a
/// crawl wander into every subdomain of the target. Ports are ignored, and
/// the root may be given as a URL or a bare host. A target whose host cannot
/// be read is refused rather than assumed to match.
pub fn decide(
    c: &CrawlConfig,
    url: &str,
    depth: usize,
    root_domain: &str,
    seen: &[String],
    fetched: usize,
) -> Decision {
    if depth > c.max_depth {
        return Decision::SkipDepth;
    }
    if c.exclude.iter().any(|p| glob_match(p, url)) {
        return Decision::SkipFilter;
    }
    if !c.include.is_empty() && !c.include.iter().any(|p| glob_match(p, url)) {
        return Decision::SkipFilter;
    }
    if c.same_domain {
        let root = normalize_host(root_domain);
        match (normalize_host(url), root) {
            (Some(host), Some(root)) if host == root => {}
            _ => return Decision::SkipDomain,
        }
    }
    if seen.iter().any(|s| s == url) {
        return Decision::SkipSeen;
    }
    if fetched >= c.max_pages {
        return Decision::SkipPageCap;
    }
    Decision::Fetch
}

/// The first crawl tier, in deterministic order: `start_url` (depth 0) first,
/// then each URL of `discovered` (depth 1) in **input order**, each kept only
/// when `decide` returns `Fetch`. The tier stops at `max_pages`, counting the
/// start URL.
///
/// Duplicates collapse through the seen rule — the accepted list is threaded
/// back into `decide` as `seen` — so nothing is re-sorted and no `HashSet`
/// iteration order can leak in: the same inputs always produce the same
/// frontier. A URL skipped for any reason is simply absent; the frontier
/// never carries an entry the fetcher would refuse.
pub fn frontier(
    c: &CrawlConfig,
    root_domain: &str,
    start_url: &str,
    discovered: &[String],
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (i, url) in std::iter::once(start_url)
        .chain(discovered.iter().map(String::as_str))
        .enumerate()
    {
        if out.len() >= c.max_pages {
            break;
        }
        let depth = usize::from(i > 0);
        if decide(c, url, depth, root_domain, &out, out.len()) == Decision::Fetch {
            out.push(url.to_string());
        }
    }
    out
}

/// The host of `s`, lowercased and port-free, or `None` when no host can be
/// read — an unreadable target must never silently pass a domain check.
///
/// A URL is read as `scheme://[userinfo@]host[:port]/…` (a leading `//` also
/// works), so a userinfo, a port, a query and a fragment are all dropped. A
/// string without a scheme is accepted only as a bare `host[:port]`,
/// IPv6 included in its brackets (`Example.COM.`, `example.com:8443`,
/// `[::1]`) — a bare string carrying a path, a userinfo or a non-numeric `:`
/// is a URL that cannot be read, and is refused rather than guessed at, which
/// is what keeps `mailto:x@example.com` from passing as `example.com`.
fn normalize_host(s: &str) -> Option<String> {
    let s = s.trim();
    let authority = if let Some(i) = s.find("://") {
        s[i + 3..].split(['/', '?', '#']).next().unwrap_or_default()
    } else if let Some(rest) = s.strip_prefix("//") {
        rest.split(['/', '?', '#']).next().unwrap_or_default()
    } else if is_bare_host(s) {
        s
    } else {
        return None;
    };
    let host_port = match authority.rsplit_once('@') {
        Some((_userinfo, host)) => host,
        None => authority,
    };
    let host = match host_port.strip_prefix('[') {
        Some(rest) => rest.split_once(']').map(|(host, _)| host)?,
        None => host_port.split(':').next().unwrap_or_default(),
    };
    let host = host.trim_end_matches('.');
    if host.is_empty() {
        return None;
    }
    Some(host.to_lowercase())
}

/// Whether `s` is a bare `host[:port]`: letters, digits, `.`, `-` and `_`
/// (or a bracketed IPv6), with an optional all-digit port.
fn is_bare_host(s: &str) -> bool {
    if let Some(rest) = s.strip_prefix('[') {
        return match rest.split_once(']') {
            Some((inner, tail)) => {
                !inner.is_empty()
                    && (tail.is_empty() || tail.strip_prefix(':').is_some_and(is_port))
            }
            None => false,
        };
    }
    let (host, port) = match s.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (s, None),
    };
    !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && port.map_or(true, is_port)
}

/// A port: digits only, at least one.
fn is_port(p: &str) -> bool {
    !p.is_empty() && p.chars().all(|c| c.is_ascii_digit())
}

/// The glob match used by `include`/`exclude`, anchored on the whole string:
///
/// - `*` matches a run of characters **not** containing `/`, so it stays
///   inside one path segment,
/// - `**` matches any run of characters including `/`, and `/**/` also
///   matches nothing at all (`a/**/b` matches `a/b` as well as `a/x/y/b`),
/// - `?` matches exactly one character; every other character is literal.
///
/// Matching walks `char`s, never bytes, so a pattern can never split a
/// multi-byte character.
fn glob_match(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    glob_match_at(&pattern, &text)
}

fn glob_match_at(p: &[char], t: &[char]) -> bool {
    let Some(&head) = p.first() else {
        return t.is_empty();
    };
    match head {
        '*' => {
            let double = p.get(1) == Some(&'*');
            let rest = if double { &p[2..] } else { &p[1..] };
            // `/**/` matches zero segments, so `a/**/b` also matches `a/b`.
            if double && rest.first() == Some(&'/') && glob_match_at(&rest[1..], t) {
                return true;
            }
            for i in 0..=t.len() {
                if glob_match_at(rest, &t[i..]) {
                    return true;
                }
                if !double && t.get(i) == Some(&'/') {
                    break;
                }
            }
            false
        }
        '?' => !t.is_empty() && glob_match_at(&p[1..], &t[1..]),
        c => t.first() == Some(&c) && glob_match_at(&p[1..], &t[1..]),
    }
}

// ── firecrawl endpoints ──────────────────────────────────────────────────

/// Inclusive upper bound on a firecrawl `limit`.
pub const MAX_FIRECRAWL_LIMIT: usize = 100;

/// The endpoint vocabulary as `Endpoint::parse`'s error lists it.
const ENDPOINT_VOCAB: &str =
    "scrape, crawl, map, search (or their paths /v1/scrape, /v1/crawl, /v1/map, /v1/search)";

/// The four firecrawl endpoints this port models.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Endpoint {
    Scrape,
    Crawl,
    Map,
    Search,
}

impl Endpoint {
    /// Every endpoint, in the order the API introduced them (and the order
    /// the error messages list them in).
    pub const ALL: [Endpoint; 4] = [
        Endpoint::Scrape,
        Endpoint::Crawl,
        Endpoint::Map,
        Endpoint::Search,
    ];

    /// The bare name, which `parse` also accepts.
    pub fn as_str(self) -> &'static str {
        match self {
            Endpoint::Scrape => "scrape",
            Endpoint::Crawl => "crawl",
            Endpoint::Map => "map",
            Endpoint::Search => "search",
        }
    }

    /// The wire path on the API.
    pub fn path(self) -> &'static str {
        match self {
            Endpoint::Scrape => "/v1/scrape",
            Endpoint::Crawl => "/v1/crawl",
            Endpoint::Map => "/v1/map",
            Endpoint::Search => "/v1/search",
        }
    }

    /// Parse an endpoint from config text. Accepts the bare name, the path,
    /// and their case/`-`/`_` variants (`/v1/Scrape`, `SCRAPE`, `v1_map`):
    /// the comparison drops separators and a `v1` version segment, so a
    /// hand-written config does not have to guess the spelling. Anything else
    /// is an error that quotes the value and lists the vocabulary, which is
    /// the whole repair.
    pub fn parse(s: &str) -> Result<Self, String> {
        let filtered: String = s
            .trim()
            .to_lowercase()
            .chars()
            .filter(|c| *c != '-' && *c != '_')
            .collect();
        let trimmed = filtered.trim_matches('/');
        let key = match trimmed.strip_prefix("v1") {
            Some(rest) => rest.trim_start_matches('/'),
            None => trimmed,
        };
        match key {
            "scrape" => Ok(Endpoint::Scrape),
            "crawl" => Ok(Endpoint::Crawl),
            "map" => Ok(Endpoint::Map),
            "search" => Ok(Endpoint::Search),
            "" => Err(format!(
                "empty firecrawl endpoint (`{s}`) — expected one of {ENDPOINT_VOCAB}"
            )),
            other => Err(format!(
                "unknown firecrawl endpoint `{other}` (from `{s}`) — expected one of \
                 {ENDPOINT_VOCAB}"
            )),
        }
    }
}

/// A response format a request can ask for. The declaration order **is** the
/// canonical order `firecrawl_body` emits: a body is a wire artifact, so the
/// same request must serialize identically every time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Format {
    Markdown,
    Html,
    Links,
    Screenshot,
}

impl Format {
    /// The wire string for this format.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Markdown => "markdown",
            Format::Html => "html",
            Format::Links => "links",
            Format::Screenshot => "screenshot",
        }
    }
}

/// One firecrawl request as the caller declares it, before it becomes a body.
#[derive(Debug, Clone, PartialEq)]
pub struct FirecrawlRequest {
    pub endpoint: Endpoint,
    /// Required by `Scrape`/`Crawl`/`Map`; forbidden for `Search`.
    pub url: Option<String>,
    /// Required by `Search`; forbidden for every other endpoint.
    pub query: Option<String>,
    /// At least one format, deduplicated and canonically ordered in the body.
    pub formats: Vec<Format>,
    /// `Crawl`/`Search` only; clamped into 1..=100.
    pub limit: Option<usize>,
}

/// The exact JSON body for `r` — **only** the keys its endpoint defines, so a
/// body built here can never carry an invented field a server would have to
/// guess about:
///
/// | endpoint | keys |
/// | --- | --- |
/// | `Scrape` | `url`, `formats` |
/// | `Crawl` | `url`, `formats`, `limit` |
/// | `Map` | `url`, `formats` |
/// | `Search` | `query`, `formats`, `limit` |
///
/// `limit` appears only when `Some` — absent means the server's own default.
///
/// The field checks are refusals, not defaults: a `url` on `Search` or a
/// `query` on `Scrape`/`Crawl`/`Map` is a caller wiring the wrong field, and
/// guessing which one was meant would send a request nobody asked for.
///
/// `formats` must be non-empty (a request with no format returns nothing
/// usable); duplicates collapse, since asking twice for the same format is
/// the same request. A declared `limit` is clamped into 1..=100 rather than
/// rejected, because a page count is a spend knob and the clamp is the bound
/// the caller asked for; `limit` anywhere but `Crawl`/`Search` is an error.
pub fn firecrawl_body(r: &FirecrawlRequest) -> Result<Value, String> {
    let mut body = Map::new();
    match r.endpoint {
        Endpoint::Scrape | Endpoint::Crawl | Endpoint::Map => {
            let url = match r.url.as_deref() {
                Some(url) if !url.trim().is_empty() => url,
                _ => {
                    return Err(format!(
                        "`url` is required for the {} endpoint — a request with no target has \
                         nothing to fetch",
                        r.endpoint.as_str()
                    ))
                }
            };
            if r.query.is_some() {
                return Err(format!(
                    "`query` is only defined for the Search endpoint — {} takes `url`",
                    r.endpoint.as_str()
                ));
            }
            body.insert("url".to_string(), Value::String(url.to_string()));
        }
        Endpoint::Search => {
            if r.url.is_some() {
                return Err(
                    "`url` must not be given for the Search endpoint — search takes a `query`; a \
                     URL here is a caller bug, not a default to fall back on"
                        .to_string(),
                );
            }
            let query =
                match r.query.as_deref() {
                    Some(query) if !query.trim().is_empty() => query,
                    _ => return Err(
                        "`query` is required for the search endpoint — a search with no query has \
                         nothing to look for"
                            .to_string(),
                    ),
                };
            body.insert("query".to_string(), Value::String(query.to_string()));
        }
    }

    if r.formats.is_empty() {
        return Err(format!(
            "`formats` must not be empty for the {} endpoint — a request with no format returns \
             nothing usable",
            r.endpoint.as_str()
        ));
    }
    let mut formats = r.formats.clone();
    formats.sort();
    formats.dedup();
    body.insert(
        "formats".to_string(),
        Value::Array(
            formats
                .into_iter()
                .map(|f| Value::String(f.as_str().to_string()))
                .collect(),
        ),
    );

    if let Some(limit) = r.limit {
        match r.endpoint {
            Endpoint::Crawl | Endpoint::Search => {
                body.insert(
                    "limit".to_string(),
                    Value::from(limit.clamp(1, MAX_FIRECRAWL_LIMIT)),
                );
            }
            _ => {
                return Err(format!(
                    "`limit` is only defined for the Crawl and Search endpoints — {} takes none",
                    r.endpoint.as_str()
                ))
            }
        }
    }

    Ok(Value::Object(body))
}

// ── scrapy request/response chain ────────────────────────────────────────

/// One declared chain step: `name` is the identity the trace reports and
/// `enabled` is the operator's off switch.
///
/// The asymmetry this port exists for: **request** chains run in declared
/// order, **response** chains run in reverse — the innermost middleware is
/// the first to see a response, so it is the last to see a request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChainStep {
    pub name: String,
    pub enabled: bool,
}

/// Validate a chain's identities: every step name must be non-blank and
/// unique, because the name is what an error and a trace report. Nothing
/// about the steps' *effects* is checked here — that needs an input value, so
/// the runners own it and report the offending step.
pub fn validate_chain(steps: &[ChainStep]) -> Result<(), String> {
    let mut seen: Vec<&str> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        if step.name.trim().is_empty() {
            return Err(format!(
                "chain step at index {i} has a blank name — the name is the identity an error \
                 reports, so it cannot be empty"
            ));
        }
        if seen.contains(&step.name.as_str()) {
            return Err(format!(
                "duplicate chain step name `{}` at index {i} — two steps sharing one name cannot \
                 be told apart in a trace",
                step.name
            ));
        }
        seen.push(step.name.as_str());
    }
    Ok(())
}

/// A built-in step that sets one key of a JSON object — the "add a flag" step
/// every middleware chain has.
#[derive(Debug, Clone, PartialEq)]
pub struct SetKey {
    pub key: String,
    pub value: Value,
}

impl SetKey {
    /// Set `key` on an object input, replacing any previous value.
    ///
    /// A non-object input is an error naming the key and the input's kind: a
    /// set can only add a key where keys exist, and wrapping the value or
    /// overwriting a scalar would silently change the shape every following
    /// step sees.
    pub fn apply(&self, v: Value) -> Result<Value, String> {
        match v {
            Value::Object(mut map) => {
                map.insert(self.key.clone(), self.value.clone());
                Ok(Value::Object(map))
            }
            other => Err(format!(
                "`set {}` needs a JSON object input, got {}",
                self.key,
                kind_of(&other)
            )),
        }
    }
}

/// A built-in step that renames one key of a JSON object — the "fix the field
/// name between the two shapes" step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenameKey {
    pub from: String,
    pub to: String,
}

impl RenameKey {
    /// Rename `from` to `to`.
    ///
    /// A missing `from` is an error that names it and lists the keys that are
    /// present, because a rename of a key that is not there means the step is
    /// wired to the wrong field — creating `to` anyway would hide that for as
    /// long as the crawl runs. An already-present `to` is overwritten: the
    /// rename is the last word on that name, which is what makes a `rename`
    /// following a `set` of the same key deterministic.
    pub fn apply(&self, v: Value) -> Result<Value, String> {
        match v {
            Value::Object(mut map) => match map.remove(&self.from) {
                Some(value) => {
                    map.insert(self.to.clone(), value);
                    Ok(Value::Object(map))
                }
                None => Err(format!(
                    "`rename {} -> {}` found no key `{}` to rename (present keys: {})",
                    self.from,
                    self.to,
                    self.from,
                    key_list(&map)
                )),
            },
            other => Err(format!(
                "`rename {} -> {}` needs a JSON object input, got {}",
                self.from,
                self.to,
                kind_of(&other)
            )),
        }
    }
}

/// The built-in step vocabulary as it appears in a chain name:
///
/// - `set:<key>=<json>` — set `key` to the JSON literal after `=`, so
///   `set:source="crawl"` sets a string and `set:limit=25` a number,
/// - `rename:<from>:<to>` — rename one key; `from` runs to the first `:`,
///   `to` takes the rest.
///
/// The operation travels in the name because a chain is *data* (it loads from
/// config) and the runner takes no code registry — this is the whole
/// vocabulary, and anything else is refused with the grammar quoted back.
/// Names are taken literally: no trimming, so a key with a space is the key
/// the operator wrote.
enum Op {
    Set(SetKey),
    Rename(RenameKey),
}

fn parse_op(name: &str) -> Result<Op, String> {
    if let Some(rest) = name.strip_prefix("set:") {
        let (key, raw) = rest
            .split_once('=')
            .ok_or_else(|| format!("step `{name}`: a set step must read `set:<key>=<json>`"))?;
        if key.is_empty() {
            return Err(format!("step `{name}`: a set step needs a key before `=`"));
        }
        let value: Value = serde_json::from_str(raw)
            .map_err(|e| format!("step `{name}`: the value after `=` is not JSON ({e})"))?;
        return Ok(Op::Set(SetKey {
            key: key.to_string(),
            value,
        }));
    }
    if let Some(rest) = name.strip_prefix("rename:") {
        let (from, to) = rest.split_once(':').ok_or_else(|| {
            format!("step `{name}`: a rename step must read `rename:<from>:<to>`")
        })?;
        if from.is_empty() || to.is_empty() {
            return Err(format!(
                "step `{name}`: a rename step needs both a `from` and a `to` key"
            ));
        }
        return Ok(Op::Rename(RenameKey {
            from: from.to_string(),
            to: to.to_string(),
        }));
    }
    Err(format!(
        "step `{name}`: unknown step — a chain step reads `set:<key>=<json>` or \
         `rename:<from>:<to>`"
    ))
}

/// Apply the enabled steps in **declared order** (request middleware). Each
/// step's output is the next step's input, so the order is the semantics.
///
/// The chain stops at the first error, and the error names the failing step
/// and the direction — a chain that reported only "key missing" would leave
/// the operator guessing which of ten middlewares did it. Disabled steps are
/// skipped before their name is even parsed, so disabling a broken step is a
/// working recovery.
pub fn run_request_chain(steps: &[ChainStep], input: Value) -> Result<Value, String> {
    run_chain(steps, input, false)
}

/// The same, in **reverse order**: response middleware nests, so the last
/// declared middleware is the first to see the response.
pub fn run_response_chain(steps: &[ChainStep], input: Value) -> Result<Value, String> {
    run_chain(steps, input, true)
}

fn run_chain(steps: &[ChainStep], input: Value, reverse: bool) -> Result<Value, String> {
    let direction = if reverse { "response" } else { "request" };
    validate_chain(steps).map_err(|e| format!("{direction} chain: {e}"))?;
    let order: Vec<&ChainStep> = if reverse {
        steps.iter().rev().collect()
    } else {
        steps.iter().collect()
    };
    let mut value = input;
    for step in order {
        if !step.enabled {
            continue;
        }
        let op = parse_op(&step.name).map_err(|e| format!("{direction} chain {e}"))?;
        value = match op {
            Op::Set(set) => set.apply(value),
            Op::Rename(rename) => rename.apply(value),
        }
        .map_err(|e| format!("{direction} chain step `{}`: {e}", step.name))?;
    }
    Ok(value)
}

/// The JSON kind of `v`, for error messages.
fn kind_of(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

/// The present keys of `map`, shortened and capped, for error messages.
fn key_list(map: &Map<String, Value>) -> String {
    let mut names: Vec<String> = map.keys().take(8).map(|k| short(k)).collect();
    if names.is_empty() {
        return "<none>".to_string();
    }
    if map.len() > names.len() {
        names.push("…".to_string());
    }
    names.join(", ")
}

/// `s` cut to 24 characters (char counts, never bytes), with an ellipsis when
/// it was cut.
fn short(s: &str) -> String {
    if s.chars().count() > 24 {
        format!("{}…", s.chars().take(24).collect::<String>())
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config() -> CrawlConfig {
        CrawlConfig {
            max_depth: 2,
            max_pages: 3,
            include: Vec::new(),
            exclude: Vec::new(),
            same_domain: true,
            delay_ms: 250,
            concurrency: 4,
        }
    }

    fn urls(items: &[&str]) -> Vec<String> {
        items.iter().map(|u| u.to_string()).collect()
    }

    fn keys(body: &Value) -> Vec<String> {
        let mut keys: Vec<String> = body.as_object().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    #[test]
    fn crawl_bounds_name_the_field_and_its_range() {
        let ok = config();
        assert_eq!(validate_crawl(&ok), Ok(()));

        let err = validate_crawl(&CrawlConfig {
            max_depth: MAX_CRAWL_DEPTH + 1,
            ..ok.clone()
        })
        .unwrap_err();
        assert!(err.contains("max_depth") && err.contains("0..=10"), "{err}");

        for pages in [0, MAX_CRAWL_PAGES + 1] {
            let err = validate_crawl(&CrawlConfig {
                max_pages: pages,
                ..ok.clone()
            })
            .unwrap_err();
            assert!(
                err.contains("max_pages") && err.contains("1..=1000"),
                "{pages}: {err}"
            );
        }

        for workers in [0, MAX_CRAWL_CONCURRENCY + 1] {
            let err = validate_crawl(&CrawlConfig {
                concurrency: workers,
                ..ok.clone()
            })
            .unwrap_err();
            assert!(
                err.contains("concurrency") && err.contains("1..=16"),
                "{workers}: {err}"
            );
        }

        // The boundary values themselves are inside the ranges.
        assert_eq!(
            validate_crawl(&CrawlConfig {
                max_depth: MAX_CRAWL_DEPTH,
                max_pages: MAX_CRAWL_PAGES,
                concurrency: MAX_CRAWL_CONCURRENCY,
                ..ok.clone()
            }),
            Ok(())
        );

        // delay_ms is politeness, not a bound: any value passes.
        assert_eq!(
            validate_crawl(&CrawlConfig {
                delay_ms: u64::MAX,
                ..ok
            }),
            Ok(())
        );
    }

    #[test]
    fn crawl_globs_must_be_non_blank_and_exclude_all_is_refused() {
        let base = config();
        for blank in ["", " ", "\t"] {
            let err = validate_crawl(&CrawlConfig {
                include: vec!["/**".to_string(), blank.to_string()],
                ..base.clone()
            })
            .unwrap_err();
            assert!(err.contains("include") && err.contains("blank"), "{err}");

            let err = validate_crawl(&CrawlConfig {
                exclude: vec![blank.to_string()],
                ..base.clone()
            })
            .unwrap_err();
            assert!(err.contains("exclude") && err.contains("blank"), "{err}");
        }

        let err = validate_crawl(&CrawlConfig {
            exclude: vec!["/private/**".to_string(), "**".to_string()],
            ..base.clone()
        })
        .unwrap_err();
        assert!(err.contains("crawls nothing"), "{err}");
        assert!(err.contains("exclude"), "{err}");

        // A narrower exclude, and the same glob as an include, are both fine:
        // including everything is a no-op filter, excluding everything is not.
        assert_eq!(
            validate_crawl(&CrawlConfig {
                include: vec!["**".to_string()],
                exclude: vec!["**/admin/**".to_string()],
                ..base
            }),
            Ok(())
        );
    }

    #[test]
    fn decide_precedence_reports_the_highest_rule_that_refuses() {
        let c = CrawlConfig {
            max_depth: 1,
            max_pages: 2,
            include: vec!["**/docs/**".to_string()],
            exclude: vec!["**/docs/private/**".to_string()],
            same_domain: true,
            delay_ms: 0,
            concurrency: 1,
        };
        // Deep, excluded, off-domain, seen and over the page cap at once:
        // depth is the first rule, so depth is what is reported.
        let url = "https://other.example/docs/private/x";
        let witness = urls(&[url]);
        assert_eq!(
            decide(&c, url, 9, "example.com", &witness, 9),
            Decision::SkipDepth
        );
        // Depth fine → the filter is next, and exclude beats the include that
        // this URL does match.
        assert_eq!(
            decide(&c, url, 1, "example.com", &witness, 9),
            Decision::SkipFilter
        );
        // A URL outside the include list is filtered too.
        assert_eq!(
            decide(&c, "https://other.example/about", 1, "example.com", &[], 0),
            Decision::SkipFilter
        );
        // Filter fine → domain.
        let in_scope = "https://other.example/docs/page";
        assert_eq!(
            decide(&c, in_scope, 1, "example.com", &[], 0),
            Decision::SkipDomain
        );
        // Domain fine → seen.
        let on_site = "https://example.com/docs/page";
        assert_eq!(
            decide(&c, on_site, 1, "example.com", &urls(&[on_site]), 0),
            Decision::SkipSeen
        );
        // Seen fine → page cap.
        assert_eq!(
            decide(&c, on_site, 1, "example.com", &[], 2),
            Decision::SkipPageCap
        );
        // One below the cap, nothing seen: fetch.
        assert_eq!(
            decide(&c, on_site, 1, "example.com", &[], 1),
            Decision::Fetch
        );
    }

    #[test]
    fn domain_rule_is_exact_case_insensitive_host_equality() {
        let mut c = config();
        let root = "example.com";

        // Same host, however it is spelled: scheme, case, port, userinfo,
        // path and a trailing root dot do not matter.
        for url in [
            "https://example.com/a",
            "HTTP://Example.COM/a?b#c",
            "https://example.com:8443/a",
            "https://user:pw@example.com/a",
            "https://example.com./a",
        ] {
            assert_eq!(decide(&c, url, 0, root, &[], 0), Decision::Fetch, "{url}");
        }

        // A subdomain is a different site: `.<root>` suffix matching is NOT
        // the rule, so both directions of a suffix match are rejected.
        for url in [
            "https://a.example.com/a",
            "https://docs.example.com/a",
            "https://www.example.com/a",
        ] {
            assert_eq!(
                decide(&c, url, 0, root, &[], 0),
                Decision::SkipDomain,
                "{url}"
            );
        }
        // ...including when the subdomain is the root the crawl was given.
        assert_eq!(
            decide(&c, "https://example.com/a", 0, "www.example.com", &[], 0),
            Decision::SkipDomain
        );

        // The root may be a URL, and a target with no readable host is
        // refused rather than assumed to match.
        assert_eq!(
            decide(
                &c,
                "https://example.com/a",
                0,
                "https://example.com/start",
                &[],
                0
            ),
            Decision::Fetch
        );
        for url in ["example.com/a", "mailto:x@example.com", ""] {
            assert_eq!(
                decide(&c, url, 0, root, &[], 0),
                Decision::SkipDomain,
                "{url}"
            );
        }

        // A bare host with a port is accepted on both sides (ports are
        // ignored), and IPv6 is read out of its brackets.
        assert_eq!(
            decide(&c, "https://example.com/a", 0, "Example.COM:8443", &[], 0),
            Decision::Fetch
        );
        assert_eq!(
            decide(&c, "https://[::1]/a", 0, "[::1]", &[], 0),
            Decision::Fetch
        );
        assert_eq!(
            decide(&c, "https://[::1]/a", 0, "example.com", &[], 0),
            Decision::SkipDomain
        );

        // With same_domain off, none of that applies.
        c.same_domain = false;
        assert_eq!(
            decide(&c, "https://other.test/a", 0, "", &[], 0),
            Decision::Fetch
        );
    }

    #[test]
    fn a_root_url_at_depth_zero_always_starts_a_crawl() {
        let c = CrawlConfig {
            max_depth: 0,
            max_pages: 1,
            include: vec!["**".to_string()],
            exclude: vec!["**/admin/**".to_string()],
            same_domain: true,
            delay_ms: 5,
            concurrency: 1,
        };
        assert_eq!(
            decide(&c, "https://example.com/", 0, "example.com", &[], 0),
            Decision::Fetch
        );
        // A nested URL at depth 0 is still an ordinary filter decision: the
        // exclude refuses it, and depth is not what did it.
        assert_eq!(
            decide(&c, "https://example.com/admin/x", 0, "example.com", &[], 0),
            Decision::SkipFilter
        );
    }

    #[test]
    fn frontier_is_start_then_discovered_in_input_order_up_to_the_cap() {
        let c = CrawlConfig {
            max_depth: 1,
            max_pages: 3,
            include: vec!["https://example.com/docs/**".to_string()],
            exclude: Vec::new(),
            same_domain: true,
            delay_ms: 0,
            concurrency: 1,
        };
        let discovered = urls(&[
            "https://example.com/docs/b",
            "https://example.com/about",
            "https://other.example/docs/c",
            "https://example.com/docs/d",
        ]);
        let start = "https://example.com/docs/a";
        assert_eq!(
            frontier(&c, "example.com", start, &discovered),
            urls(&[
                start,
                "https://example.com/docs/b",
                "https://example.com/docs/d",
            ]),
            "start first, then discovered in input order, stopped at max_pages=3"
        );

        // The URL that would have followed the cap also qualified, proving
        // the stop — not the filter — ended the tier.
        let more = urls(&[
            "https://example.com/docs/b",
            "https://example.com/docs/d",
            "https://example.com/docs/e",
        ]);
        assert_eq!(frontier(&c, "example.com", start, &more).len(), 3);

        // max_depth bounds the tier: at depth 0 only the start survives,
        // because everything discovered is depth 1.
        let shallow = CrawlConfig {
            max_depth: 0,
            ..c.clone()
        };
        assert_eq!(
            frontier(&shallow, "example.com", start, &discovered),
            urls(&[start])
        );

        // An unfetchable start yields an empty frontier; an empty discovered
        // list yields just the start.
        assert!(frontier(&c, "other.example", start, &[]).is_empty());
        assert_eq!(frontier(&c, "example.com", start, &[]), urls(&[start]));
    }

    #[test]
    fn frontier_collapses_duplicates_without_reordering() {
        let c = CrawlConfig {
            max_depth: 1,
            max_pages: 10,
            include: Vec::new(),
            exclude: Vec::new(),
            same_domain: true,
            delay_ms: 0,
            concurrency: 1,
        };
        let start = "https://example.com/a";
        let discovered = urls(&[
            "https://example.com/b",
            start,
            "https://example.com/b",
            "https://example.com/c",
        ]);
        // The repeated start URL and the repeated /b collapse through the
        // seen rule; the survivors keep input order (b before c).
        assert_eq!(
            frontier(&c, "example.com", start, &discovered),
            urls(&[start, "https://example.com/b", "https://example.com/c"])
        );
    }

    #[test]
    fn glob_rules_separate_segments_and_collapse_globstars() {
        // `*` never crosses a `/`.
        assert!(glob_match("https://e.test/*", "https://e.test/a"));
        assert!(!glob_match("https://e.test/*", "https://e.test/a/b"));
        // `**` does, and `/**/` matches zero segments.
        assert!(glob_match("https://e.test/**", "https://e.test/a/b"));
        assert!(glob_match("https://e.test/**/b", "https://e.test/a/x/b"));
        assert!(glob_match("https://e.test/**/b", "https://e.test/b"));
        assert!(!glob_match("https://e.test/**/b", "https://e.test/a/x/c"));
        // Anchored on the whole string; `?` is one character.
        assert!(!glob_match("**/docs/**", "https://e.test/guides/a"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        // Multi-byte text is matched by char, never split by byte.
        assert!(glob_match("caf?", "café"));
        assert!(glob_match("**/café/**", "https://e.test/café/x"));
    }

    #[test]
    fn endpoint_paths_and_parse_variants_agree() {
        assert_eq!(
            Endpoint::ALL.map(|e| e.path()),
            ["/v1/scrape", "/v1/crawl", "/v1/map", "/v1/search"]
        );
        assert_eq!(
            Endpoint::ALL.map(|e| e.as_str()),
            ["scrape", "crawl", "map", "search"]
        );
        for e in Endpoint::ALL {
            // The bare name, the path, and the case/separator variants all
            // land on the same endpoint.
            for s in [
                e.as_str().to_string(),
                e.as_str().to_uppercase(),
                format!(" {}", e.as_str()),
                e.path().to_string(),
                e.path().to_uppercase(),
                format!("{}/", e.path()),
                e.path().trim_start_matches('/').replace('/', "_"),
                e.as_str()
                    .chars()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join("-"),
                format!("v1/{}", e.as_str()),
            ] {
                assert_eq!(Endpoint::parse(&s), Ok(e), "{s:?}");
            }
        }
        // Everything else is refused, quoting the value and listing the
        // vocabulary.
        for bad in ["", " ", "/", "v1", "sitemap", "scraped", "search/v1"] {
            let err = Endpoint::parse(bad).unwrap_err();
            assert!(
                err.contains("expected one of") && err.contains("/v1/scrape"),
                "{bad:?}: {err}"
            );
        }
        assert!(Endpoint::parse("sitemap").unwrap_err().contains("sitemap"));
    }

    #[test]
    fn scraped_body_keys_are_exactly_url_and_formats() {
        let body = firecrawl_body(&FirecrawlRequest {
            endpoint: Endpoint::Scrape,
            url: Some("https://example.com/a".to_string()),
            query: None,
            formats: vec![
                Format::Screenshot,
                Format::Markdown,
                Format::Screenshot,
                Format::Html,
            ],
            limit: None,
        })
        .unwrap();
        assert_eq!(keys(&body), ["formats", "url"]);
        assert_eq!(
            body["formats"],
            json!(["markdown", "html", "screenshot"]),
            "deduped, and in canonical order rather than request order"
        );
        assert_eq!(body["url"], json!("https://example.com/a"));
    }

    #[test]
    fn url_is_required_where_it_is_defined_and_refused_where_it_is_not() {
        let req = |endpoint, url: Option<&str>, query: Option<&str>| FirecrawlRequest {
            endpoint,
            url: url.map(str::to_string),
            query: query.map(str::to_string),
            formats: vec![Format::Markdown],
            limit: None,
        };

        for endpoint in [Endpoint::Scrape, Endpoint::Crawl, Endpoint::Map] {
            let err = firecrawl_body(&req(endpoint, None, None)).unwrap_err();
            assert!(
                err.contains("url") && err.contains(endpoint.as_str()),
                "{err}"
            );
            let err = firecrawl_body(&req(endpoint, Some("   "), None)).unwrap_err();
            assert!(err.contains("`url` is required"), "{err}");
            // The wrong field is refused, not ignored.
            let err =
                firecrawl_body(&req(endpoint, Some("https://e.test"), Some("q"))).unwrap_err();
            assert!(err.contains("`query` is only defined"), "{err}");
        }

        // Search: a URL is a caller bug, and a blank query is not a query.
        let err =
            firecrawl_body(&req(Endpoint::Search, Some("https://e.test"), Some("q"))).unwrap_err();
        assert!(err.contains("`url` must not be given"), "{err}");
        let err = firecrawl_body(&req(Endpoint::Search, None, None)).unwrap_err();
        assert!(err.contains("`query` is required"), "{err}");
        assert!(firecrawl_body(&req(Endpoint::Search, None, Some(" ")))
            .unwrap_err()
            .contains("`query` is required"));
    }

    #[test]
    fn limit_is_crawl_and_search_only_and_is_clamped() {
        let req = |endpoint, limit: Option<usize>| FirecrawlRequest {
            endpoint,
            url: Some("https://e.test/a".to_string()),
            query: None,
            formats: vec![Format::Markdown],
            limit,
        };
        // Not defined for Scrape or Map, whatever the value.
        for endpoint in [Endpoint::Scrape, Endpoint::Map] {
            let err = firecrawl_body(&req(endpoint, Some(5))).unwrap_err();
            assert!(
                err.contains("`limit` is only defined") && err.contains(endpoint.as_str()),
                "{err}"
            );
        }
        // Crawl clamps both ends…
        for (given, clamped) in [(0usize, 1usize), (1, 1), (100, 100), (250, 100)] {
            let body = firecrawl_body(&req(Endpoint::Crawl, Some(given))).unwrap();
            assert_eq!(body["limit"], json!(clamped), "limit {given}");
        }
        // …and so does Search, while an absent limit stays absent.
        let search = FirecrawlRequest {
            url: None,
            query: Some("rust crawling".to_string()),
            ..req(Endpoint::Search, Some(5))
        };
        assert_eq!(firecrawl_body(&search).unwrap()["limit"], json!(5));
        assert!(firecrawl_body(&FirecrawlRequest {
            limit: None,
            ..search
        })
        .unwrap()
        .get("limit")
        .is_none());
        assert!(firecrawl_body(&req(Endpoint::Crawl, None))
            .unwrap()
            .get("limit")
            .is_none());
    }

    #[test]
    fn each_endpoint_body_carries_only_its_own_keys() {
        let req = |endpoint: Endpoint, with_limit: bool| FirecrawlRequest {
            endpoint,
            url: match endpoint {
                Endpoint::Search => None,
                _ => Some("https://e.test/a".to_string()),
            },
            query: match endpoint {
                Endpoint::Search => Some("rust crawling".to_string()),
                _ => None,
            },
            formats: vec![Format::Html, Format::Links],
            limit: with_limit.then_some(7),
        };

        assert_eq!(
            keys(&firecrawl_body(&req(Endpoint::Scrape, false)).unwrap()),
            ["formats", "url"]
        );
        assert_eq!(
            keys(&firecrawl_body(&req(Endpoint::Map, false)).unwrap()),
            ["formats", "url"]
        );
        // Crawl's `limit` is present only when it was declared.
        assert_eq!(
            keys(&firecrawl_body(&req(Endpoint::Crawl, false)).unwrap()),
            ["formats", "url"]
        );
        assert_eq!(
            keys(&firecrawl_body(&req(Endpoint::Crawl, true)).unwrap()),
            ["formats", "limit", "url"]
        );
        // Search carries `query`, never `url`.
        assert_eq!(
            keys(&firecrawl_body(&req(Endpoint::Search, false)).unwrap()),
            ["formats", "query"]
        );
        assert_eq!(
            keys(&firecrawl_body(&req(Endpoint::Search, true)).unwrap()),
            ["formats", "limit", "query"]
        );
    }

    #[test]
    fn formats_must_not_be_empty() {
        for endpoint in Endpoint::ALL {
            let err = firecrawl_body(&FirecrawlRequest {
                endpoint,
                url: match endpoint {
                    Endpoint::Search => None,
                    _ => Some("https://e.test/a".to_string()),
                },
                query: match endpoint {
                    Endpoint::Search => Some("rust".to_string()),
                    _ => None,
                },
                formats: Vec::new(),
                limit: None,
            })
            .unwrap_err();
            assert!(
                err.contains("`formats` must not be empty") && err.contains(endpoint.as_str()),
                "{err}"
            );
        }
    }

    fn step(name: &str) -> ChainStep {
        ChainStep {
            name: name.to_string(),
            enabled: true,
        }
    }

    #[test]
    fn chain_validation_rejects_blank_and_duplicate_names() {
        assert_eq!(validate_chain(&[]), Ok(()));
        assert_eq!(
            validate_chain(&[step("set:a=1"), step("rename:a:b")]),
            Ok(())
        );

        let err = validate_chain(&[step("set:a=1"), step("  "), step("set:b=2")]).unwrap_err();
        assert!(err.contains("blank") && err.contains("index 1"), "{err}");

        let err = validate_chain(&[step("set:a=1"), step("set:a=1")]).unwrap_err();
        assert!(
            err.contains("duplicate") && err.contains("set:a=1"),
            "{err}"
        );
        assert!(err.contains("index 1"), "{err}");
    }

    #[test]
    fn request_chains_run_forward_and_response_chains_run_reverse() {
        // Two order-sensitive steps over the same input: whichever runs last
        // owns the surviving value of `b`.
        let steps = [step("set:b=2"), step("rename:a:b")];
        let input = || json!({"a": 1});
        // Request order: set b=2, then rename a -> b — the rename wins.
        assert_eq!(run_request_chain(&steps, input()).unwrap(), json!({"b": 1}));
        // Response order: rename a -> b, then set b=2 — the set wins.
        assert_eq!(
            run_response_chain(&steps, input()).unwrap(),
            json!({"b": 2})
        );

        // The same asymmetry with a single key and no collision at all.
        let writes = [step("set:k=\"first\""), step("set:k=\"second\"")];
        assert_eq!(
            run_request_chain(&writes, json!({})).unwrap(),
            json!({"k": "second"})
        );
        assert_eq!(
            run_response_chain(&writes, json!({})).unwrap(),
            json!({"k": "first"})
        );

        // Three writers of one key, to prove the reversal is total and not a
        // swap: forward ends on the third write, reverse on the first, and a
        // swap would leave the middle one.
        let three = ["set:k=\"1\"", "set:k=\"2\"", "set:k=\"3\""].map(step);
        assert_eq!(
            run_request_chain(&three, json!({})).unwrap(),
            json!({"k": "3"})
        );
        assert_eq!(
            run_response_chain(&three, json!({})).unwrap(),
            json!({"k": "1"})
        );
    }

    #[test]
    fn a_disabled_step_is_skipped_before_its_name_is_read() {
        let steps = [
            step("set:a=1"),
            ChainStep {
                name: "rename:a:b".to_string(),
                enabled: false,
            },
            // Malformed, but disabled: skipping a broken step is the
            // documented recovery, so it cannot fail the chain.
            ChainStep {
                name: "not-a-step".to_string(),
                enabled: false,
            },
        ];
        for out in [
            run_request_chain(&steps, json!({})).unwrap(),
            run_response_chain(&steps, json!({})).unwrap(),
        ] {
            assert_eq!(out, json!({"a": 1}));
        }
        // Enabling it changes the answer, which is what makes the skip real.
        let enabled = [step("set:a=1"), step("rename:a:b")];
        assert_eq!(
            run_request_chain(&enabled, json!({})).unwrap(),
            json!({"b": 1})
        );
    }

    #[test]
    fn rename_names_the_key_it_could_not_find() {
        let rename = RenameKey {
            from: "content".to_string(),
            to: "body".to_string(),
        };
        let err = rename.apply(json!({"title": "x"})).unwrap_err();
        assert!(err.contains("content") && err.contains("title"), "{err}");
        assert_eq!(
            rename.apply(json!({"content": 1})).unwrap(),
            json!({"body": 1})
        );
        // An existing target is overwritten by the rename.
        assert_eq!(
            rename.apply(json!({"content": 1, "body": 2})).unwrap(),
            json!({"body": 1})
        );
        // A non-object input is refused with the kind named.
        assert!(rename
            .apply(json!([1, 2]))
            .unwrap_err()
            .contains("needs a JSON object input, got an array"));

        // Through the runner, the same failure names the step that did it.
        let err =
            run_request_chain(&[step("rename:content:body")], json!({"title": "x"})).unwrap_err();
        assert!(
            err.contains("request chain step `rename:content:body`") && err.contains("content"),
            "{err}"
        );
        let err = run_response_chain(&[step("rename:content:body")], json!({})).unwrap_err();
        assert!(err.contains("response chain step"), "{err}");
    }

    #[test]
    fn set_needs_an_object_and_reports_the_input_kind() {
        let set = SetKey {
            key: "source".to_string(),
            value: json!("crawl"),
        };
        assert_eq!(
            set.apply(json!({"a": 1})).unwrap(),
            json!({"a": 1, "source": "crawl"})
        );
        // Setting an existing key replaces it.
        assert_eq!(
            set.apply(json!({"source": "old"})).unwrap(),
            json!({"source": "crawl"})
        );
        for (input, kind) in [
            (json!("s"), "a string"),
            (json!(1), "a number"),
            (json!(null), "null"),
            (json!(true), "a boolean"),
        ] {
            let err = set.apply(input).unwrap_err();
            assert!(err.contains("source") && err.contains(kind), "{err}");
        }
    }

    #[test]
    fn a_chain_stops_at_the_first_error_and_names_that_step() {
        let steps = [
            step("set:a=1"),
            step("rename:missing:x"),
            // This one would also fail: the error must be the earlier step's.
            step("rename:also_missing:y"),
        ];
        let err = run_request_chain(&steps, json!({})).unwrap_err();
        assert!(err.contains("rename:missing:x"), "{err}");
        assert!(!err.contains("also_missing"), "stopped at the first: {err}");
        assert!(err.contains("present keys: a"), "lists what exists: {err}");
    }

    #[test]
    fn unknown_or_malformed_step_names_are_refused_with_the_grammar() {
        let err = |name: &str| run_request_chain(&[step(name)], json!({})).unwrap_err();

        let message = err("http.compression");
        assert!(
            message.contains("http.compression") && message.contains("set:<key>=<json>"),
            "{message}"
        );
        assert!(err("set:novalue").contains("set:<key>=<json>"));
        assert!(err("set:=1").contains("needs a key before `=`"));
        let message = err("set:a={not json}");
        assert!(
            message.contains("step `set:a={not json}`") && message.contains("not JSON"),
            "{message}"
        );
        assert!(err("rename:onlyone").contains("rename:<from>:<to>"));
        assert!(err("rename::to").contains("both a `from` and a `to`"));
        assert!(err("rename:from:").contains("both a `from` and a `to`"));
    }

    #[test]
    fn set_values_are_json_literals_not_strings() {
        let steps = [
            step("set:n=25"),
            step("set:flag=true"),
            step("set:tags=[\"a\",\"b\"]"),
            step("set:wrap={\"k\":1}"),
        ];
        assert_eq!(
            run_request_chain(&steps, json!({})).unwrap(),
            json!({"n": 25, "flag": true, "tags": ["a", "b"], "wrap": {"k": 1}})
        );
    }

    #[test]
    fn a_chain_is_revalidated_before_it_runs() {
        let steps = [step("set:a=1"), step("set:a=1")];
        let err = run_request_chain(&steps, json!({})).unwrap_err();
        assert!(
            err.contains("duplicate") && err.contains("request chain"),
            "{err}"
        );
        assert!(run_response_chain(&steps, json!({}))
            .unwrap_err()
            .contains("response chain"));
    }

    #[test]
    fn long_keys_are_truncated_by_char_for_error_messages() {
        let rename = RenameKey {
            from: "absent".to_string(),
            to: "x".to_string(),
        };
        let with_key = |key: &str| {
            let mut map = Map::new();
            map.insert(key.to_string(), json!(1));
            Value::Object(map)
        };
        let long = "k".repeat(40);
        let err = rename.apply(with_key(&long)).unwrap_err();
        assert!(err.contains(&format!("{}…", "k".repeat(24))), "{err}");
        assert!(!err.contains(&long), "the key list is capped: {err}");
        // A multi-byte key is cut on a char boundary, never mid-character.
        let unicode = "é".repeat(40);
        let err = rename.apply(with_key(&unicode)).unwrap_err();
        assert!(err.contains(&format!("{}…", "é".repeat(24))), "{err}");
    }

    #[test]
    fn a_long_key_list_is_capped_with_a_marker() {
        let mut map = Map::new();
        for i in 0..12 {
            map.insert(format!("k{i}"), json!(i));
        }
        let err = RenameKey {
            from: "absent".to_string(),
            to: "x".to_string(),
        }
        .apply(Value::Object(map))
        .unwrap_err();
        assert!(err.contains("k0") && err.contains("…"), "{err}");
        assert!(!err.contains("k8") && !err.contains("k9"), "capped: {err}");
        // No keys at all reads as such rather than as an empty list.
        assert!(RenameKey {
            from: "absent".to_string(),
            to: "x".to_string()
        }
        .apply(json!({}))
        .unwrap_err()
        .contains("<none>"));
    }
}
