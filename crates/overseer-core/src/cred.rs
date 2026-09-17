//! Credential broker (P6-3, playbook Ch.11 §5.5 lean spine).
//!
//! The broker holds real secrets process-side and hands the model only
//! opaque sentinels (`ovsent_` + 32 lowercase hex — frozen contract shared
//! with P7's channel sentinel). Real values are injected into `bash`
//! children as env vars; every `ToolResult` text is verbatim-sanitized
//! back to sentinels AFTER the taint latch sees the raw text, so the
//! latch, the model context, AND `events.jsonl` all see the safe form by
//! construction (the event append copies the same sanitized string the
//! model reads).
//!
//! Reals are never serialized: `Credential::real` has no Serialize path,
//! `Broker` has a redacting Debug, and scans redact spills before write.

use std::collections::HashMap;

/// Sentinel prefix — frozen contract (both branches, R2-F6).
pub const SENTINEL_PREFIX: &str = "ovsent_";
/// Hex body length after the prefix.
pub const SENTINEL_HEX_LEN: usize = 32;

/// One brokered credential: the selector names the env var the child
/// sees; the sentinel is what the model sees; `real` never leaves the
/// process except into a spawned child's env.
#[derive(Clone)]
pub struct Credential {
    pub id: String,
    pub selector: String,
    pub sentinel: String,
    pub inject_hosts: Vec<String>,
    pub scopes: Vec<String>,
    pub expires_ms: Option<u64>,
    real: String,
}

impl std::fmt::Debug for Credential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Reals never render — sentinel only.
        f.debug_struct("Credential")
            .field("id", &self.id)
            .field("selector", &self.selector)
            .field("sentinel", &self.sentinel)
            .field("real", &"[redacted]")
            .finish()
    }
}

impl Credential {
    /// Build a credential, deriving a deterministic sentinel from
    /// (id, real) via sha256 hex (first 32 chars). Same (id, real) →
    /// same sentinel (1:1 mapping the tests pin).
    pub fn new(
        id: impl Into<String>,
        selector: impl Into<String>,
        real: impl Into<String>,
        inject_hosts: Vec<String>,
        scopes: Vec<String>,
        expires_ms: Option<u64>,
    ) -> Self {
        let id = id.into();
        let real = real.into();
        let sentinel = sentinel_for(&id, &real);
        Credential {
            id,
            selector: selector.into(),
            sentinel,
            inject_hosts,
            scopes,
            expires_ms,
            real,
        }
    }

    /// Server-side only: the real secret. Marker-named so call sites
    /// read as privileged (`real_for` mirrors it on the broker).
    pub fn real_secret(&self) -> &str {
        &self.real
    }
}

/// Deterministic sentinel for (id, real): sha256, first 32 hex chars.
pub fn sentinel_for(id: &str, real: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"overseer-cred-v1:");
    h.update(id.as_bytes());
    h.update(b":");
    h.update(real.as_bytes());
    let hex: String = h
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{SENTINEL_PREFIX}{}", &hex[..SENTINEL_HEX_LEN])
}

/// True when `s` is exactly a sentinel (`ovsent_` + 32 lowercase hex).
pub fn is_sentinel(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != SENTINEL_PREFIX.len() + SENTINEL_HEX_LEN {
        return false;
    }
    if &s[..SENTINEL_PREFIX.len()] != SENTINEL_PREFIX {
        return false;
    }
    b[SENTINEL_PREFIX.len()..]
        .iter()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// The broker: id → credential. Default-empty; cloned into tool contexts
/// that need injection/sanitization.
#[derive(Default, Clone)]
pub struct Broker {
    map: HashMap<String, Credential>,
}

impl std::fmt::Debug for Broker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Reals never render: ids + sentinels only.
        let ids: Vec<(&str, &str)> = self
            .map
            .values()
            .map(|c| (c.id.as_str(), c.sentinel.as_str()))
            .collect();
        f.debug_struct("Broker").field("creds", &ids).finish()
    }
}

impl Broker {
    pub fn new() -> Self {
        Broker {
            map: HashMap::new(),
        }
    }

    /// Register a credential; returns its sentinel (the only value the
    /// model ever sees).
    pub fn issue_capability(
        &mut self,
        id: impl Into<String>,
        selector: impl Into<String>,
        real: impl Into<String>,
        inject_hosts: Vec<String>,
        scopes: Vec<String>,
        expires_ms: Option<u64>,
    ) -> String {
        let cred = Credential::new(id, selector, real, inject_hosts, scopes, expires_ms);
        let s = cred.sentinel.clone();
        self.map.insert(cred.id.clone(), cred);
        s
    }

    /// (selector, real) env pairs for bash child injection.
    pub fn inject_env(&self) -> Vec<(String, String)> {
        self.map
            .values()
            .map(|c| (c.selector.clone(), c.real_secret().to_string()))
            .collect()
    }

    /// Server-side only: real secret for `id`. Call sites are the bash
    /// injector and the sanitizer — never the model path.
    pub fn real_for(&self, id: &str) -> Option<&str> {
        self.map.get(id).map(|c| c.real_secret())
    }

    pub fn sentinel_for_id(&self, id: &str) -> Option<&str> {
        self.map.get(id).map(|c| c.sentinel.as_str())
    }

    pub fn get(&self, id: &str) -> Option<&Credential> {
        self.map.get(id)
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// (real, sentinel) pairs for sanitize/inject loops.
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.map
            .values()
            .map(|c| (c.real_secret(), c.sentinel.as_str()))
            .collect()
    }
}

/// Verbatim real→sentinel rewrite over tool-result text. Longest reals
/// first (a real that contains another still maps 1:1). Pure string
/// replace — no regex, no allocation beyond the result.
pub fn sanitize(broker: &Broker, text: &str) -> String {
    let mut pairs = broker.pairs();
    pairs.sort_by_key(|(real, _)| std::cmp::Reverse(real.len()));
    let mut out = text.to_string();
    for (real, sentinel) in pairs {
        if real.is_empty() {
            continue;
        }
        if out.contains(real) {
            out = out.replace(real, sentinel);
        }
    }
    out
}

/// One redaction span: byte range + family name. Span-only (no content
/// copied into the notice/metadata log).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redaction {
    pub start: usize,
    pub end: usize,
    pub family: &'static str,
}

/// Curated secret families (regex-lite hand-rolls, no new deps):
/// `AKIA…` keys, `gh[ps]…`/`github_pat_` tokens, `xox[bpas]-` Slack
/// tokens, `sk-live-`/`sk-test-` keys, PEM blocks, plus high-entropy
/// long tokens (≥20 chars, entropy ≥ 3.5). FP allowlist: `EXAMPLE`,
/// `TEST-ONLY`, `...`, `xxx` (case-insensitive) suppress a span.
pub fn scan(text: &str) -> Vec<Redaction> {
    let b = text.as_bytes();
    let mut spans: Vec<Redaction> = Vec::new();
    macro_rules! push {
        ($start:expr, $end:expr, $family:expr) => {{
            let (s, e): (usize, usize) = ($start, $end);
            if e > s && e <= text.len() && !is_fp(&text[s..e]) {
                spans.push(Redaction {
                    start: s,
                    end: e,
                    family: $family,
                });
            }
        }};
    }
    // AKIA + 16 uppercase alnum.
    for i in 0..b.len().saturating_sub(20) {
        if &text[i..i + 4] == "AKIA"
            && text[i + 4..i + 20]
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        {
            push!(i, i + 20, "aws-key");
        }
    }
    // ghp_/gho_/ghs_/ghu_/github_pat_.
    for pat in ["ghp_", "gho_", "ghs_", "ghu_", "github_pat_"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(pat) {
            let s = from + rel;
            let mut e = s + pat.len();
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'_') && e - s < 120 {
                e += 1;
            }
            if e - s >= pat.len() + 8 {
                push!(s, e, "github-token");
            }
            from = e.max(s + 1);
            if from >= b.len() {
                break;
            }
        }
    }
    // xox[bpas]- + token body.
    for pat in ["xoxb-", "xoxp-", "xoxa-", "xoxs-"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(pat) {
            let s = from + rel;
            let mut e = s + pat.len();
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'-') && e - s < 120 {
                e += 1;
            }
            if e - s >= pat.len() + 8 {
                push!(s, e, "slack-token");
            }
            from = e.max(s + 1);
            if from >= b.len() {
                break;
            }
        }
    }
    // sk-live- / sk-test- + body.
    for pat in ["sk-live-", "sk-test-"] {
        let mut from = 0;
        while let Some(rel) = text[from..].find(pat) {
            let s = from + rel;
            let mut e = s + pat.len();
            while e < b.len() && (b[e].is_ascii_alphanumeric() || b[e] == b'-' || b[e] == b'_') {
                e += 1;
            }
            if e - s >= pat.len() + 8 {
                push!(s, e, "api-key");
            }
            from = e.max(s + 1);
            if from >= b.len() {
                break;
            }
        }
    }
    // PEM blocks: -----BEGIN ...----- … -----END ...-----.
    let mut from = 0;
    while let Some(rel) = text[from..].find("-----BEGIN") {
        let s = from + rel;
        if let Some(end_rel) = text[s..].find("-----END") {
            let tail = &text[s + end_rel..];
            let line_end = tail.find('\n').map(|n| s + end_rel + n).unwrap_or(text.len());
            let e = (s + end_rel + "-----END".len()).max(line_end.min(text.len()));
            let e = e.min(text.len());
            push!(s, e.max(s + 10), "pem-block");
            from = e.max(s + 1);
        } else {
            push!(s, text.len().min(s + 64), "pem-block");
            break;
        }
        if from >= b.len() {
            break;
        }
    }
    // High-entropy long tokens: ≥20 alnum/+/=/_/- chars with entropy ≥3.5.
    let mut i = 0;
    while i < b.len() {
        if is_token_char(b[i]) {
            let s = i;
            while i < b.len() && is_token_char(b[i]) {
                i += 1;
            }
            if i - s >= 20
                && !spans.iter().any(|r| r.start <= s && s < r.end)
                && shannon(&text[s..i]) >= 3.5
            {
                push!(s, i, "high-entropy");
            }
        } else {
            i += 1;
        }
    }
    // Sort + drop spans fully contained in an earlier (longer) span.
    spans.sort_by_key(|r| (r.start, std::cmp::Reverse(r.end)));
    let mut out: Vec<Redaction> = Vec::new();
    for r in spans {
        if out
            .iter()
            .any(|o: &Redaction| o.start <= r.start && r.end <= o.end)
        {
            continue;
        }
        out.push(r);
    }
    out
}

fn is_token_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'+' || c == b'/' || c == b'='
}

fn is_fp(frag: &str) -> bool {
    let u = frag.to_uppercase();
    u.contains("EXAMPLE") || u.contains("TEST-ONLY") || u.contains("TEST_ONLY") || frag.contains("...") || u.contains("XXX")
}

/// Shannon entropy (bits/char) over the fragment's bytes.
pub fn shannon(s: &str) -> f64 {
    let b = s.as_bytes();
    if b.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for c in b {
        counts[*c as usize] += 1;
    }
    let n = b.len() as f64;
    counts
        .iter()
        .filter(|c| **c > 0)
        .map(|c| {
            let p = f64::from(*c) / n;
            -p * p.log2()
        })
        .sum()
}

/// Apply `scan` redactions: each span → `[redacted:<family>]`, plus a
/// one-line notice naming families only (never content). Returns
/// (redacted_text, notice_or_empty).
pub fn redact(text: &str) -> (String, String) {
    let spans = scan(text);
    if spans.is_empty() {
        return (text.to_string(), String::new());
    }
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    let mut fams: Vec<&str> = Vec::new();
    for r in &spans {
        if r.start < last {
            continue;
        }
        out.push_str(&text[last..r.start]);
        out.push_str(&format!("[redacted:{}]", r.family));
        if !fams.contains(&r.family) {
            fams.push(r.family);
        }
        last = r.end;
    }
    out.push_str(&text[last..]);
    let notice = format!("[overseer] redacted {} secret span(s): {}", spans.len(), fams.join(", "));
    (out, notice)
}

/// Metadata log for scan redactions: stderr line naming span count +
/// families only (never content). The spill path calls this so the
/// redaction itself stays auditable without copying secrets.
pub fn note_redaction(notice: &str) {
    eprintln!("{notice}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sentinel_format_is_frozen_contract() {
        // `ovsent_` + 32 lowercase hex (shared with P7's channel sentinel).
        let s = sentinel_for("db", "s3cr3t-real");
        assert!(s.starts_with(SENTINEL_PREFIX), "{s}");
        assert_eq!(s.len(), SENTINEL_PREFIX.len() + SENTINEL_HEX_LEN);
        assert!(is_sentinel(&s), "{s}");
        // Rejections: uppercase, short, wrong prefix, trailing junk.
        assert!(!is_sentinel("ovsent_ABCD1234abcd1234abcd1234abcd12"));
        assert!(!is_sentinel("ovsent_abc123"));
        assert!(!is_sentinel("bearer_abcdef1234567890abcdef1234567890"));
        assert!(!is_sentinel(&format!("{s}X")));
    }

    #[test]
    fn sentinel_mapping_is_one_to_one() {
        // Same (id, real) → same sentinel; different real → different.
        let a1 = sentinel_for("db", "real-1");
        let a2 = sentinel_for("db", "real-1");
        let b = sentinel_for("db", "real-2");
        let c = sentinel_for("other", "real-1");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        assert_ne!(a1, c);
        let mut br = Broker::new();
        let s = br.issue_capability("db", "DB_PASS", "real-1", vec![], vec![], None);
        assert_eq!(s, a1);
        assert_eq!(br.real_for("db"), Some("real-1"));
    }

    #[test]
    fn reals_never_serialize_or_debug() {
        let mut br = Broker::new();
        br.issue_capability("db", "DB_PASS", "super-secret-real", vec![], vec![], None);
        let dbg = format!("{br:?}");
        assert!(!dbg.contains("super-secret-real"), "{dbg}");
        assert!(dbg.contains("ovsent_"), "{dbg}");
        let cred = br.get("db").unwrap();
        // Credential exposes no Serialize impl: only Debug (derived) —
        // assert the Debug form redacts by construction (field is private,
        // so serde can't see it either without a manual impl).
        let cdbg = format!("{cred:?}");
        assert!(!cdbg.contains("super-secret-real"), "{cdbg}");
    }

    #[test]
    fn scan_families_fp_and_entropy() {
        // Each curated family fires.
        assert!(scan("key AKIAIOSFODNN7QWERTY12 here").iter().any(|r| r.family == "aws-key"));
        // …except the FP allowlist suppresses EXAMPLE-bearing spans.
        assert!(scan("key AKIAIOSFODNN7EXAMPLE here").is_empty());
        assert!(
            scan("token ghp_abcdefgh12345678 here")
                .iter()
                .any(|r| r.family == "github-token")
        );
        assert!(
            scan("token xoxb-123456789012-abcdefgh here")
                .iter()
                .any(|r| r.family == "slack-token")
        );
        assert!(
            scan("key sk-live-abcdefgh12345678 here")
                .iter()
                .any(|r| r.family == "api-key")
        );
        assert!(
            scan("-----BEGIN PRIVATE KEY-----\nabc\n-----END PRIVATE KEY-----")
                .iter()
                .any(|r| r.family == "pem-block")
        );
        // High entropy ≥3.5 fires; low-entropy runs don't.
        assert!(
            scan("tok aB3dE5gH7jK9mN2pQ4rS6tU8vW here")
                .iter()
                .any(|r| r.family == "high-entropy")
        );
        assert!(scan("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").is_empty());
        assert!(scan("hello world, nothing secret here").is_empty());
        // FPfamilies: TEST-ONLY / xxx / ... suppress.
        assert!(scan("token ghp_TEST-ONLY-abcdefgh12345678 here").is_empty());
        assert!(scan("key sk-live-xxxxxxxxxxxxxxxxxx here").is_empty());
    }

    #[test]
    fn injection_child_sees_real_log_sees_sentinel() {
        // Broker pairs drive both halves: injector maps selector→real,
        // sanitize maps real→sentinel. Pin the round trip here.
        let mut br = Broker::new();
        let s = br.issue_capability(
            "api",
            "API_TOKEN",
            "tok-real-123",
            vec!["api.example.com".into()],
            vec!["read".into()],
            None,
        );
        let child_env = format!("API_TOKEN={}", br.real_for("api").unwrap());
        assert!(child_env.contains("tok-real-123"));
        let log = sanitize(&br, &format!("called with {child_env}"));
        assert!(!log.contains("tok-real-123"), "{log}");
        assert!(log.contains(&s), "{log}");
    }

    #[test]
    fn no_plaintext_in_tool_result_path() {
        // Verbatim sanitize over a ToolResult-shaped string.
        let mut br = Broker::new();
        let s = br.issue_capability("db", "DB_PASS", "pw-real-9", vec![], vec![], None);
        let text = "query ok, password was pw-real-9 done";
        let clean = sanitize(&br, text);
        assert!(!clean.contains("pw-real-9"));
        assert!(clean.contains(&s));
        // Idempotent: sanitizing twice is stable.
        assert_eq!(sanitize(&br, &clean), clean);
    }
}
