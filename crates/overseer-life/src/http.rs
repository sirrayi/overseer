//! Bounded JSON fetcher (port of Synara `providerUsage/http.ts` onto
//! ureq). Every call pins an origin allow-list, a global timeout and a
//! response-size cap, and a per-run request budget enforces the
//! "at most one request per source per run" live-probe rule. Errors never
//! carry header values or response bodies.

use std::cell::Cell;
use std::time::Duration;

use serde_json::Value;
use thiserror::Error;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_RESPONSE_BYTES: u64 = 1024 * 1024;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

#[derive(Debug, Error)]
pub enum HttpError {
    /// The request URL's origin is not on the call's allow-list.
    #[error("origin not allowed for this call")]
    OriginNotAllowed,
    /// Plain HTTP is refused unless the target is loopback AND the ctx
    /// opted into loopback (test-only).
    #[error("refusing non-https request")]
    NotHttps,
    /// The run's request budget is spent; the request was never sent.
    #[error("request budget for this run is exhausted")]
    BudgetExhausted,
    /// URL or request could not be built.
    #[error("bad request")]
    BadRequest,
    #[error("timeout")]
    Timeout,
    #[error("response exceeded the size cap")]
    TooLarge,
    #[error("transport failed")]
    Transport,
    #[error("malformed response body")]
    Malformed,
}

impl HttpError {
    /// The snapshot error kind this maps to.
    pub fn error_kind(&self) -> crate::snapshot::ErrorKind {
        match self {
            HttpError::Timeout => crate::snapshot::ErrorKind::Timeout,
            HttpError::Malformed | HttpError::TooLarge => crate::snapshot::ErrorKind::Malformed,
            _ => crate::snapshot::ErrorKind::Transport,
        }
    }
}

#[derive(Debug)]
pub struct Fetched {
    pub status: u16,
    /// Parsed JSON body, or None when the body was empty/not JSON.
    pub json: Option<Value>,
    /// Response headers (lower-cased names). Returned for the connector's
    /// own parsing (Codex x-codex-* windows); never embedded in errors.
    pub headers: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Method {
    Get,
    Post,
}

pub enum Body {
    Json(Value),
    Form(Vec<(String, String)>),
}

pub struct Request<'a> {
    pub url: &'a str,
    pub method: Method,
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Option<Body>,
    /// Origin the URL must match exactly (scheme + host + port).
    pub allowed_origin: &'a str,
}

/// `scheme://host[:port]` of a URL, or None when it cannot be parsed to
/// that much. Rejects userinfo (`@`) so a URL cannot smuggle credentials
/// past the origin check.
pub fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '-' || c == '.')
    {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next()?;
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    Some(format!(
        "{}://{}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    ))
}

/// Host part of an origin/URL (no port).
pub fn host_of(url: &str) -> Option<String> {
    let origin = origin_of(url)?;
    let authority = origin.split("://").nth(1)?;
    // Strip a trailing :port; IPv6 literals keep their brackets.
    if let Some(host) = authority.strip_prefix('[') {
        let end = host.find(']')?;
        return Some(host[..end].to_string());
    }
    Some(authority.split(':').next()?.to_string())
}

pub(crate) fn is_loopback_host(host: &str) -> bool {
    let h = host.trim_end_matches('.').to_ascii_lowercase();
    h == "localhost"
        || h == "127.0.0.1"
        || h == "::1"
        || h.ends_with(".localhost")
        || h.starts_with("127.")
}

/// Shared ureq agent + per-run counters. Cheap to clone-construct; the
/// request counter lives on `self` so one `Http` instance = one run.
pub struct Http {
    agent: ureq::Agent,
    pub allow_loopback_http: bool,
    pub max_requests: Option<u32>,
    pub timeout: Duration,
    pub max_response_bytes: u64,
    requests_made: Cell<u32>,
}

impl Http {
    /// Production configuration: no loopback http, no request cap (the
    /// probe sets `max_requests` explicitly; library callers own this).
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_millis(DEFAULT_TIMEOUT_MS)))
            .timeout_connect(Some(Duration::from_millis(5_000)))
            .max_redirects(0)
            .max_redirects_will_error(true)
            .build();
        Http {
            agent: ureq::Agent::new_with_config(config),
            allow_loopback_http: false,
            max_requests: None,
            timeout: Duration::from_millis(DEFAULT_TIMEOUT_MS),
            max_response_bytes: MAX_RESPONSE_BYTES,
            requests_made: Cell::new(0),
        }
    }

    /// Test-only configuration: loopback http allowed so connectors can be
    /// exercised against a local mock server. Kept as a named constructor
    /// so production callers cannot enable it by accident.
    #[doc(hidden)]
    pub fn for_test() -> Self {
        let mut h = Http::new();
        h.allow_loopback_http = true;
        h
    }

    pub fn requests_made(&self) -> u32 {
        self.requests_made.get()
    }

    /// Origin + scheme + budget checks, then the request. Headers the
    /// caller passes are sent verbatim (they carry the bearer token —
    /// never echoed into errors).
    pub fn fetch(&self, req: &Request) -> Result<Fetched, HttpError> {
        let origin = origin_of(req.url).ok_or(HttpError::BadRequest)?;
        if origin != req.allowed_origin {
            return Err(HttpError::OriginNotAllowed);
        }
        let https = origin.starts_with("https://");
        if !https {
            let loopback = host_of(req.url).is_some_and(|h| is_loopback_host(&h));
            if !(loopback && self.allow_loopback_http) {
                return Err(HttpError::NotHttps);
            }
        }
        if let Some(cap) = self.max_requests {
            if self.requests_made.get() >= cap {
                return Err(HttpError::BudgetExhausted);
            }
        }
        self.requests_made.set(self.requests_made.get() + 1);

        // Serialize the body once: the request-size cap applies to what
        // actually goes on the wire, for JSON and form bodies alike.
        let encoded_body = match &req.body {
            None => None,
            Some(Body::Json(v)) => Some((
                "application/json",
                serde_json::to_vec(v).map_err(|_| HttpError::BadRequest)?,
            )),
            Some(Body::Form(pairs)) => Some((
                "application/x-www-form-urlencoded",
                form_encode(pairs).into_bytes(),
            )),
        };
        if let Some((_, bytes)) = &encoded_body {
            if bytes.len() > MAX_REQUEST_BYTES {
                return Err(HttpError::BadRequest);
            }
        }

        // GET and POST builders are distinct types in ureq 3 — branch
        // rather than unify.
        let mut resp = match req.method {
            Method::Get => {
                let mut call = self
                    .agent
                    .get(req.url)
                    .config()
                    .timeout_per_call(Some(self.timeout))
                    .build();
                for (name, value) in req.headers {
                    call = call.header(*name, *value);
                }
                call.call()
            }
            Method::Post => {
                let mut call = self
                    .agent
                    .post(req.url)
                    .config()
                    .timeout_per_call(Some(self.timeout))
                    .build();
                for (name, value) in req.headers {
                    call = call.header(*name, *value);
                }
                match &encoded_body {
                    None => call.send_empty(),
                    Some((content_type, bytes)) => {
                        call = call.header("content-type", *content_type);
                        call.send(bytes.as_slice())
                    }
                }
            }
        }
        .map_err(map_ureq_error)?;

        let status = resp.status().as_u16();
        let headers: Vec<(String, String)> = resp
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|v| (name.as_str().to_ascii_lowercase(), v.to_string()))
            })
            .collect();
        let body = resp
            .body_mut()
            .with_config()
            .limit(self.max_response_bytes)
            .read_to_vec()
            .map_err(|e| {
                // A exceeded body limit surfaces as Io("body exceeds limit…").
                if matches!(&e, ureq::Error::Io(io_e) if io_e.to_string().contains("limit")) {
                    HttpError::TooLarge
                } else {
                    map_ureq_error(e)
                }
            })?;
        let json = if body.is_empty() {
            None
        } else {
            Some(serde_json::from_slice::<Value>(&body).map_err(|_| HttpError::Malformed)?)
        };
        Ok(Fetched {
            status,
            json,
            headers,
        })
    }
}

impl Default for Http {
    fn default() -> Self {
        Http::new()
    }
}

fn map_ureq_error(e: ureq::Error) -> HttpError {
    match e {
        ureq::Error::Timeout(_) => HttpError::Timeout,
        _ => HttpError::Transport,
    }
}

/// `application/x-www-form-urlencoded` bodies without a dependency:
/// alphanumerics and `-._~` pass through, everything else is %XX.
/// (Same encoding works for query strings — `?a=b` percent form.)
pub(crate) fn form_encode(pairs: &[(String, String)]) -> String {
    fn enc(s: &str) -> String {
        let mut out = String::new();
        for b in s.bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                    out.push(b as char)
                }
                _ => out.push_str(&format!("%{b:02X}")),
            }
        }
        out
    }
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", enc(k), enc(v)))
        .collect::<Vec<_>>()
        .join("&")
}

/// Port of Synara's `isAuthFailureStatus` — 401/403 mean re-auth needed.
pub fn is_auth_failure(status: u16) -> bool {
    status == 401 || status == 403
}

/// `Retry-After` in its seconds form (HTTP-date form is ignored — the
/// providers we poll only send seconds). Header names arrive lower-cased.
pub fn retry_after_secs(headers: &[(String, String)]) -> Option<u64> {
    headers
        .iter()
        .find(|(k, _)| k == "retry-after")
        .and_then(|(_, v)| v.trim().parse::<u64>().ok())
}

/// Message for a 429 response: "rate limited; retry after N s" when the
/// server sent a seconds-form Retry-After, else "rate limited".
pub fn rate_limit_message(headers: &[(String, String)]) -> String {
    match retry_after_secs(headers) {
        Some(s) => format!("rate limited; retry after {s} s"),
        None => "rate limited".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_parsing() {
        assert_eq!(
            origin_of("https://api.anthropic.com/api/oauth/usage").as_deref(),
            Some("https://api.anthropic.com")
        );
        assert_eq!(
            origin_of("http://127.0.0.1:8080/x").as_deref(),
            Some("http://127.0.0.1:8080")
        );
        assert_eq!(origin_of("https://user:pw@evil.com/x"), None);
        assert_eq!(origin_of("not a url"), None);
        assert_eq!(origin_of("ftp://host/x").as_deref(), Some("ftp://host"));
    }

    #[test]
    fn loopback_detection() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("[::1]") || is_loopback_host("::1"));
        assert!(!is_loopback_host("example.com"));
    }

    #[test]
    fn form_encoding() {
        assert_eq!(
            form_encode(&[("a b".into(), "c+d".into()), ("k".into(), "v/1".into())]),
            "a%20b=c%2Bd&k=v%2F1"
        );
    }
}
