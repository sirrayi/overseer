//! X API v2 connector: OAuth 2.0 Authorization Code + PKCE for our own
//! tokens (refresh IS allowed here — R1 only protects other apps'
//! logins), a pluggable TokenStore, follower pagination that stops at the
//! first known id, and a per-UTC-day cost ledger that refuses requests
//! over the configured cap before they are sent.
//!
//! Endpoints/scopes verified against
//! <https://docs.x.com/fundamentals/authentication/oauth-2-0/authorization-code>
//! (read 2026-10-05): authorize `https://x.com/i/oauth2/authorize`,
//! token `https://api.x.com/2/oauth2/token`, scopes `tweet.read
//! users.read follows.read offline.access` (offline.access is what makes
//! the refresh token come back at all).
//!
//! Prices verified against <https://docs.x.com/x-api/getting-started/pricing>
//! (read 2026-10-05): post read $0.005/resource, user read
//! $0.010/resource, owned read $0.001/resource — the owned rate applies
//! ONLY to the twelve `/2/users/{id}/…` endpoints in
//! [`OWNED_USER_ENDPOINTS`], and only when `{id}` is the authenticated
//! user who also owns the developer app. `/2/users/me` is a plain
//! "User: Read" — it is NOT an owned endpoint.
// DEFERRED(lead): confirm metering with a live debit — Phase 0 X spike

use std::cell::Cell;
use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::connector::{Connector, ConnectorInfo, Ctx, Discovered};
use crate::creds::{self, base64url_encode};
use crate::http::{form_encode, is_auth_failure, Body, HttpError, Method, Request};
use crate::snapshot::{
    as_f64, as_non_negative, as_str, ErrorKind, Metric, Provenance, ProvenanceKind, Snapshot,
    SourceId, Status,
};
use crate::time::utc_day;
use sha2::{Digest, Sha256};

pub const AUTHORIZE_URL: &str = "https://x.com/i/oauth2/authorize";
pub const TOKEN_URL: &str = "https://api.x.com/2/oauth2/token";
pub const API_BASE: &str = "https://api.x.com";
/// Minimum scope set for counts + follower reads. `offline.access` is
/// required for X to issue a refresh token at all.
pub const DEFAULT_SCOPES: [&str; 4] =
    ["tweet.read", "users.read", "follows.read", "offline.access"];

// Per-resource prices from the docs.x.com pricing page (micro-USD).
pub const OWNED_READ_MICRO_USD: u64 = 1_000; // $0.001
pub const USER_READ_MICRO_USD: u64 = 10_000; // $0.010
pub const POST_READ_MICRO_USD: u64 = 5_000; // $0.005
/// The twelve `/2/users/{id}/…` resources billed at the owned-read rate —
/// and only when `{id}` is the authenticated user AND that user owns the
/// developer app (docs.x.com/x-api/getting-started/pricing, 2026-10-05).
/// `/2/users/me` is deliberately absent: it is a "User: Read".
const OWNED_USER_ENDPOINTS: [&str; 12] = [
    "tweets",
    "mentions",
    "liked_tweets",
    "bookmarks",
    "followers",
    "following",
    "blocking",
    "muting",
    "owned_lists",
    "followed_lists",
    "list_memberships",
    "pinned_lists",
];
/// Default daily cap: $1.00. Raise via `OVS_LIFE_X_DAILY_CAP_USD`.
const DEFAULT_DAILY_CAP_MICRO_USD: u64 = 1_000_000;
const FOLLOWERS_MAX_RESULTS: u32 = 1000;

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct XTokens {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at_ms: Option<i64>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// Keychain-backed storage lands in Phase 1; Phase 0 tokens live in
/// memory only, for one process run (see examples/x_spike.rs).
pub trait TokenStore {
    fn load(&self) -> Option<XTokens>;
    fn save(&self, tokens: &XTokens);
}

#[derive(Default)]
pub struct MemoryTokenStore {
    inner: std::cell::RefCell<Option<XTokens>>,
}

impl TokenStore for MemoryTokenStore {
    fn load(&self) -> Option<XTokens> {
        self.inner.borrow().clone()
    }

    fn save(&self, tokens: &XTokens) {
        *self.inner.borrow_mut() = Some(tokens.clone());
    }
}

// ---------------------------------------------------------------------------
// PKCE
// ---------------------------------------------------------------------------

/// Entropy bytes for PKCE verifiers/state. Fails closed: a weak
/// fallback (time/pid mixing) would produce guessable OAuth state, so
/// an unreadable /dev/urandom is an error, never a degraded value.
fn random_bytes(n: usize) -> std::io::Result<Vec<u8>> {
    use std::io::Read;
    let mut f = std::fs::File::open("/dev/urandom")?;
    let mut buf = vec![0u8; n];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

/// S256 PKCE pair + CSRF state.
pub fn pkce_pair() -> std::io::Result<PkcePair> {
    let verifier = base64url_encode(&random_bytes(32)?);
    let challenge = base64url_encode(&Sha256::digest(verifier.as_bytes()));
    let state = base64url_encode(&random_bytes(16)?);
    Ok(PkcePair {
        verifier,
        challenge,
        state,
    })
}

/// `https://x.com/i/oauth2/authorize?…` for the code flow, with the
/// default scope set (`offline.access` included — refresh tokens).
pub fn authorize_url(client_id: &str, redirect_uri: &str, pkce: &PkcePair) -> String {
    authorize_url_with_scopes(client_id, redirect_uri, pkce, &DEFAULT_SCOPES)
}

/// Same authorize URL with an explicit scope list. The spike omits
/// `offline.access`: it revokes and discards the token, so a refresh
/// token would be issued for nothing.
pub fn authorize_url_with_scopes(
    client_id: &str,
    redirect_uri: &str,
    pkce: &PkcePair,
    scopes: &[&str],
) -> String {
    let q = form_encode(&[
        ("response_type".into(), "code".into()),
        ("client_id".into(), client_id.into()),
        ("redirect_uri".into(), redirect_uri.into()),
        ("scope".into(), scopes.join(" ")),
        ("state".into(), pkce.state.clone()),
        ("code_challenge".into(), pkce.challenge.clone()),
        ("code_challenge_method".into(), "S256".into()),
    ]);
    format!("{AUTHORIZE_URL}?{q}")
}

// ---------------------------------------------------------------------------
// Cost ledger
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PriceClass {
    OwnedRead,
    UserRead,
    PostRead,
}

impl PriceClass {
    pub fn micro_usd(self) -> u64 {
        match self {
            PriceClass::OwnedRead => OWNED_READ_MICRO_USD,
            PriceClass::UserRead => USER_READ_MICRO_USD,
            PriceClass::PostRead => POST_READ_MICRO_USD,
        }
    }
}

/// What the caller knows about who is calling: the authenticated user's
/// id and whether that user also owns the developer app. Both are needed
/// for the owned-read rate; when either is unknown the classification
/// stays at the conservative (higher) public rate.
#[derive(Clone, Debug, Default)]
pub struct XPricing {
    /// The authenticated user's numeric id (the `data.id` of
    /// `/2/users/me`). `None` = unknown → never owned-priced.
    pub self_id: Option<String>,
    /// True only when the developer app is owned by the authenticated
    /// user. Default false — conservative.
    pub app_owned_by_user: bool,
}

/// Classify a request path into its billing class.
fn price_class(path: &str, pricing: &XPricing) -> PriceClass {
    // The endpoint tail of `/2/users/{id}/<ep>` (None for /2/users/me and
    // bare lookups).
    let user_ep = path
        .strip_prefix("/2/users/")
        .and_then(|rest| rest.split_once('/'))
        .map(|(_, tail)| tail.split(['?', '&']).next().unwrap_or(tail));
    if pricing.app_owned_by_user {
        if let (Some(self_id), Some(ep)) = (pricing.self_id.as_deref(), user_ep) {
            let owned_prefix = format!("/2/users/{self_id}/");
            if path.starts_with(&owned_prefix) && OWNED_USER_ENDPOINTS.contains(&ep) {
                return PriceClass::OwnedRead;
            }
        }
    }
    match user_ep {
        // These endpoints return posts, billed per post resource.
        Some("tweets") | Some("mentions") | Some("liked_tweets") | Some("bookmarks") => {
            PriceClass::PostRead
        }
        // followers/following/blocking/muting return users; the *lists
        // endpoints return lists — conservatively priced at user rate.
        Some(_) => PriceClass::UserRead,
        // /2/users/me, /2/users/{id}, /2/users/by* — user reads.
        None if path.starts_with("/2/users") => PriceClass::UserRead,
        _ => PriceClass::PostRead,
    }
}

/// Per-UTC-day spend ledger in micro-USD — per process in Phase 0.
/// Checked BEFORE each request with the worst-case cost (max_results ×
/// unit price); over the cap the request is refused and never sent.
// DEFERRED(lead): persist the X spend ledger in life-store so the daily
// cap spans processes — Phase 1
pub struct CostLedger {
    pub cap_micro_usd: u64,
    day: Cell<i64>,
    spent: Cell<u64>,
}

impl CostLedger {
    pub fn new(cap_micro_usd: u64, now_ms: i64) -> Self {
        CostLedger {
            cap_micro_usd,
            day: Cell::new(utc_day(now_ms)),
            spent: Cell::new(0),
        }
    }

    fn rollover(&self, now_ms: i64) {
        let today = utc_day(now_ms);
        if today != self.day.get() {
            self.day.set(today);
            self.spent.set(0);
        }
    }

    /// Worst-case charge for a request that could return `max_results`
    /// resources.
    pub fn estimate(path: &str, max_results: u32, pricing: &XPricing) -> u64 {
        price_class(path, pricing).micro_usd() * u64::from(max_results.max(1))
    }

    /// Returns Err(needed) when the request would exceed today's cap.
    pub fn charge(
        &self,
        now_ms: i64,
        path: &str,
        max_results: u32,
        pricing: &XPricing,
    ) -> Result<(), u64> {
        self.rollover(now_ms);
        let est = Self::estimate(path, max_results, pricing);
        let new_total = self.spent.get() + est;
        if new_total > self.cap_micro_usd {
            return Err(est);
        }
        self.spent.set(new_total);
        Ok(())
    }

    pub fn spent_micro_usd(&self) -> u64 {
        self.spent.get()
    }

    pub fn day(&self) -> i64 {
        self.day.get()
    }
}

// ---------------------------------------------------------------------------
// API calls
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum XError {
    NeedsAuth,
    Expired,
    BudgetExceeded {
        cap: u64,
        spent: u64,
    },
    Http(HttpError),
    /// HTTP 429 — carries the server's Retry-After seconds when present.
    RateLimited(Option<u64>),
    Status(u16),
    Malformed,
    /// The OAuth redirect landed but carried no usable code.
    Callback(String),
}

impl XError {
    pub fn to_status(&self) -> Status {
        match self {
            XError::NeedsAuth => Status::NeedsAuth {
                hint: "connect X with the interactive login (examples/x_spike.rs); \
                       Phase 0 keeps tokens in memory only"
                    .into(),
            },
            XError::Expired => Status::Expired,
            XError::BudgetExceeded { cap, spent } => Status::BudgetExceeded {
                cap_micro_usd: *cap,
                day_spend_micro_usd: *spent,
            },
            XError::Http(e) => Status::Error {
                kind: e.error_kind(),
                message: "could not reach the X API".into(),
            },
            XError::RateLimited(_) => Status::Error {
                kind: ErrorKind::RateLimited,
                message: match self {
                    XError::RateLimited(Some(s)) => {
                        format!("rate limited; retry after {s} s")
                    }
                    _ => "rate limited".into(),
                },
            },
            XError::Status(code) => Status::Error {
                kind: ErrorKind::Http,
                message: format!("X API request failed (HTTP {code})."),
            },
            XError::Malformed => Status::Error {
                kind: ErrorKind::Malformed,
                message: "X API response was not valid JSON.".into(),
            },
            XError::Callback(m) => Status::Error {
                kind: ErrorKind::Malformed,
                message: m.clone(),
            },
        }
    }
}

/// One fetch helper for every X call: budget check first (the request
/// is refused before being sent when over cap), bearer header, JSON body.
/// `path` is the API path + query (`/2/users/me?…`); the token rides the
/// `authorization` header only — never the URL.
fn x_get(
    ctx: &Ctx,
    ledger: &CostLedger,
    pricing: &XPricing,
    token: &str,
    path: &str,
    max_results: u32,
) -> Result<Value, XError> {
    ledger
        .charge(ctx.now_ms, path, max_results, pricing)
        .map_err(|_| XError::BudgetExceeded {
            cap: ledger.cap_micro_usd,
            spent: ledger.spent_micro_usd(),
        })?;
    // Tests rewrite the base via OVS_LIFE_URL_X_API (loopback only).
    let base = ctx
        .override_url("X_API")
        .unwrap_or_else(|| API_BASE.to_string());
    let url = format!("{base}{path}");
    let allowed = crate::http::origin_of(&url).unwrap_or_default();
    let authz = format!("Bearer {token}");
    let res = ctx
        .http
        .fetch(&Request {
            url: &url,
            method: Method::Get,
            headers: &[
                ("authorization", authz.as_str()),
                ("accept", "application/json"),
            ],
            body: None,
            allowed_origin: &allowed,
        })
        .map_err(XError::Http)?;
    if res.status == 429 {
        return Err(XError::RateLimited(crate::http::retry_after_secs(
            &res.headers,
        )));
    }
    if is_auth_failure(res.status) {
        return Err(XError::NeedsAuth);
    }
    if res.status != 200 {
        return Err(XError::Status(res.status));
    }
    res.json.ok_or(XError::Malformed)
}

/// Exchange an authorization code for tokens (token endpoint; form body).
/// Called only during the interactive login flow — never by the probe.
pub fn exchange_code(
    ctx: &Ctx,
    client_id: &str,
    redirect_uri: &str,
    code: &str,
    verifier: &str,
) -> Result<XTokens, XError> {
    token_request(
        ctx,
        &[
            ("grant_type", "authorization_code"),
            ("client_id", client_id),
            ("redirect_uri", redirect_uri),
            ("code", code),
            ("code_verifier", verifier),
        ],
    )
}

/// Refresh our own X tokens. Unlike the provider CLIs' credentials,
/// these tokens belong to this app — redemption is safe and expected.
pub fn refresh_tokens(ctx: &Ctx, client_id: &str, refresh_token: &str) -> Result<XTokens, XError> {
    token_request(
        ctx,
        &[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", client_id),
        ],
    )
}

fn token_request(ctx: &Ctx, pairs: &[(&str, &str)]) -> Result<XTokens, XError> {
    let (url, allowed) = ctx.endpoint("X_TOKEN", TOKEN_URL);
    let body: Vec<(String, String)> = pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    let res = ctx
        .http
        .fetch(&Request {
            url: &url,
            method: Method::Post,
            headers: &[("accept", "application/json")],
            body: Some(Body::Form(body)),
            allowed_origin: &allowed,
        })
        .map_err(XError::Http)?;
    if res.status == 429 {
        return Err(XError::RateLimited(crate::http::retry_after_secs(
            &res.headers,
        )));
    }
    if res.status != 200 {
        return Err(if is_auth_failure(res.status) {
            XError::NeedsAuth
        } else {
            XError::Status(res.status)
        });
    }
    let json = res.json.ok_or(XError::Malformed)?;
    let access_token = as_str(&json["access_token"])
        .ok_or(XError::Malformed)?
        .to_string();
    let expires_at_ms = as_f64(&json["expires_in"]).map(|s| ctx.now_ms + (s * 1000.0) as i64);
    Ok(XTokens {
        access_token,
        refresh_token: as_str(&json["refresh_token"]).map(str::to_string),
        expires_at_ms,
        scope: as_str(&json["scope"]).map(str::to_string),
    })
}

/// Revoke an access token when a session ends. Per
/// <https://docs.x.com/fundamentals/authentication/oauth-2-0/user-access-token>
/// (read 2026-10-05) this is POST `https://api.x.com/2/oauth2/revoke`
/// with form fields `token` and `client_id` — a public client sends no
/// secret. Best-effort: a failed revoke is not an error for the caller.
pub fn revoke_token(ctx: &Ctx, client_id: &str, token: &str) {
    // Base override works the same as x_get: the env var carries an
    // origin, not the full endpoint URL.
    let base = ctx
        .override_url("X_API")
        .unwrap_or_else(|| API_BASE.to_string());
    let url = format!("{base}/2/oauth2/revoke");
    let allowed = crate::http::origin_of(&url).unwrap_or_default();
    let _ = ctx.http.fetch(&Request {
        url: &url,
        method: Method::Post,
        headers: &[("accept", "application/json")],
        body: Some(Body::Form(vec![
            ("token".into(), token.into()),
            ("client_id".into(), client_id.into()),
        ])),
        allowed_origin: &allowed,
    });
}

/// Parse the OAuth redirect's path+query into its authorization code.
/// The caller binds the listener and supplies the request target; this
/// pure function validates it: `/callback` only, `state` must match,
/// `code` is percent-decoded.
pub fn parse_oauth_callback(path: &str, expected_state: &str) -> Result<String, XError> {
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    if route != "/callback" {
        return Err(XError::Callback("not the OAuth callback path".into()));
    }
    let mut code = None;
    let mut state = None;
    for kv in query.split('&') {
        if let Some((k, v)) = kv.split_once('=') {
            match k {
                "code" => code = Some(percent_decode(v)),
                "state" => state = Some(percent_decode(v)),
                _ => {}
            }
        }
    }
    if state.as_deref() != Some(expected_state) {
        return Err(XError::Callback(
            "OAuth state mismatch — possible CSRF, aborting".into(),
        ));
    }
    code.filter(|c| !c.is_empty())
        .ok_or_else(|| XError::Callback("OAuth callback carried no code".into()))
}

/// Percent-decode a query value (`+` → space, `%XX` → byte).
/// Forgiving: malformed escapes pass through unchanged.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hex = |b: u8| (b as char).to_digit(16);
                match (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                    (Some(hi), Some(lo)) => {
                        out.push((hi * 16 + lo) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// GET /2/users/me with public_metrics → counts snapshot. Billed as a
/// plain "User: Read" ($0.010/resource) — /2/users/me is NOT one of the
/// twelve owned endpoints; the per-request vs per-resource debit shape
/// is the open question flagged below.
// DEFERRED(lead): confirm metering with a live debit — Phase 0 X spike
pub fn counts(
    ctx: &Ctx,
    store: &dyn TokenStore,
    ledger: &CostLedger,
    pricing: &XPricing,
) -> Result<Snapshot, XError> {
    let tokens = store.load().ok_or(XError::NeedsAuth)?;
    if creds::expiry(tokens.expires_at_ms, ctx.now_ms) == creds::Expiry::Expired {
        return Err(XError::Expired);
    }
    let json = x_get(
        ctx,
        ledger,
        pricing,
        &tokens.access_token,
        "/2/users/me?user.fields=public_metrics",
        1,
    )?;

    let data = &json["data"];
    let metrics_v = &data["public_metrics"];
    let mut snap = Snapshot::new(
        SourceId::from("x"),
        ctx.now_ms,
        Status::Ok,
        Provenance {
            kind: ProvenanceKind::OfficialApi,
            endpoint: Some(format!("{API_BASE}/2/users/me")),
            documented: true,
        },
    );
    snap.account.id = as_str(&data["id"]).map(str::to_string);
    snap.account.label = as_str(&data["username"]).map(|u| format!("@{u}"));
    snap.account.fingerprint = Some(creds::fingerprint_of(&tokens.access_token));
    for (field, name) in [
        ("followers_count", "followers"),
        ("following_count", "following"),
        ("tweet_count", "posts"),
        ("listed_count", "listed"),
    ] {
        if let Some(v) = as_non_negative(&metrics_v[field]) {
            snap.metrics.push(Metric {
                name: name.into(),
                value: v,
                unit: Some("count".into()),
            });
        }
    }
    Ok(snap)
}

/// Followers page iterator: newest-first, stops when a known id appears
/// (incremental mode) or when pages run out (full scan).
#[derive(Debug)]
pub struct FollowerScan {
    /// New ids discovered (incremental) or every id seen (full scan).
    pub ids: Vec<String>,
    pub pages: u32,
    pub hit_known: bool,
}

/// One followers page (ids only, count capped by `max_results`) — what
/// the X spike uses to keep its debit to exactly one request.
/// Returns `(ids, next_token)`.
pub fn followers_page(
    ctx: &Ctx,
    store: &dyn TokenStore,
    ledger: &CostLedger,
    pricing: &XPricing,
    user_id: &str,
    max_results: u32,
) -> Result<(Vec<String>, Option<String>), XError> {
    let tokens = store.load().ok_or(XError::NeedsAuth)?;
    if creds::expiry(tokens.expires_at_ms, ctx.now_ms) == creds::Expiry::Expired {
        return Err(XError::Expired);
    }
    let path = format!(
        "/2/users/{user_id}/followers?max_results={}&user.fields=username",
        max_results.clamp(1, FOLLOWERS_MAX_RESULTS)
    );
    let json = x_get(
        ctx,
        ledger,
        pricing,
        &tokens.access_token,
        &path,
        max_results,
    )?;
    let ids = json["data"]
        .as_array()
        .map(|data| {
            data.iter()
                .filter_map(|u| as_str(&u["id"]).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let next = as_str(&json["meta"]["next_token"]).map(str::to_string);
    Ok((ids, next))
}

/// Page /2/users/{id}/followers newest-first and stop at the first
/// known id — the incremental "who's new" scan.
pub fn new_followers(
    ctx: &Ctx,
    store: &dyn TokenStore,
    ledger: &CostLedger,
    pricing: &XPricing,
    user_id: &str,
    known_ids: &HashSet<String>,
) -> Result<FollowerScan, XError> {
    scan_followers(ctx, store, ledger, pricing, user_id, Some(known_ids))
}

/// All follower pages — the full-scan used for unfollower diffs.
pub fn full_follower_scan(
    ctx: &Ctx,
    store: &dyn TokenStore,
    ledger: &CostLedger,
    pricing: &XPricing,
    user_id: &str,
) -> Result<FollowerScan, XError> {
    scan_followers(ctx, store, ledger, pricing, user_id, None)
}

fn scan_followers(
    ctx: &Ctx,
    store: &dyn TokenStore,
    ledger: &CostLedger,
    pricing: &XPricing,
    user_id: &str,
    stop_at: Option<&HashSet<String>>,
) -> Result<FollowerScan, XError> {
    let tokens = store.load().ok_or(XError::NeedsAuth)?;
    if creds::expiry(tokens.expires_at_ms, ctx.now_ms) == creds::Expiry::Expired {
        return Err(XError::Expired);
    }
    let mut out = FollowerScan {
        ids: Vec::new(),
        pages: 0,
        hit_known: false,
    };
    let mut next_token: Option<String> = None;
    loop {
        let mut path = format!(
            "/2/users/{user_id}/followers?max_results={FOLLOWERS_MAX_RESULTS}&user.fields=username"
        );
        if let Some(t) = &next_token {
            let enc = form_encode(&[("pagination_token".into(), t.clone())]);
            path.push_str(&format!("&{enc}"));
        }
        let json = x_get(
            ctx,
            ledger,
            pricing,
            &tokens.access_token,
            &path,
            FOLLOWERS_MAX_RESULTS,
        )?;
        out.pages += 1;
        let mut stop = false;
        if let Some(data) = json["data"].as_array() {
            for user in data {
                let Some(id) = as_str(&user["id"]).map(str::to_string) else {
                    continue;
                };
                // X does not document that followers pages come back
                // newest-first — stop-at-known-id is only correct if
                // they do.
                // DEFERRED(lead): confirm newest-first follower ordering
                // in the live X spike — Phase 0
                if stop_at.is_some_and(|known| known.contains(&id)) {
                    out.hit_known = true;
                    stop = true;
                    break;
                }
                out.ids.push(id);
            }
        }
        if stop {
            break;
        }
        match as_str(&json["meta"]["next_token"]).map(str::to_string) {
            Some(t) => next_token = Some(t),
            None => break,
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Connector facade — probe-facing: counts() only (R5's one request per
// source). Phase 0 has no credential discovery: tokens exist only in
// memory for one process run (the interactive login is
// examples/x_spike.rs); Keychain storage lands in Phase 1.
// ---------------------------------------------------------------------------

pub struct XConnector {
    store: Box<dyn TokenStore>,
}

impl XConnector {
    pub fn new() -> Self {
        XConnector {
            store: Box::new(MemoryTokenStore::default()),
        }
    }

    pub fn with_store(store: Box<dyn TokenStore>) -> Self {
        XConnector { store }
    }
}

impl Default for XConnector {
    fn default() -> Self {
        Self::new()
    }
}

impl Connector for XConnector {
    fn id(&self) -> SourceId {
        SourceId::from("x")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "x",
            name: "X (api.x.com v2)",
            needs_network: true,
        }
    }

    fn discover(&self, _ctx: &Ctx) -> Vec<Discovered> {
        // In-memory tokens only — nothing on disk or in the environment
        // to discover, and nothing secret to report.
        Vec::new()
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let prov = Provenance {
            kind: ProvenanceKind::OfficialApi,
            endpoint: Some(format!("{API_BASE}/2/users/me")),
            documented: true,
        };
        let Some(tokens) = self.store.load() else {
            return Snapshot::needs_auth(
                self.id(),
                ctx.now_ms,
                prov,
                "connect X with the interactive login (examples/x_spike.rs); \
                 Phase 0 keeps tokens in memory only",
            );
        };
        // R1 doesn't apply to our own tokens, but a live probe still
        // refuses: refresh needs a second request and the probe caps at 1.
        if creds::expiry(tokens.expires_at_ms, ctx.now_ms) == creds::Expiry::Expired {
            return Snapshot::expired(self.id(), ctx.now_ms, prov);
        }
        // Route the connector's snapshot through the same ledger the
        // library calls use; the probe's cap lands here too.
        let cap = ctx
            .env("OVS_LIFE_X_DAILY_CAP_USD")
            .and_then(|v| v.parse::<f64>().ok())
            .map(|usd| (usd * 1_000_000.0).max(0.0) as u64)
            .unwrap_or(DEFAULT_DAILY_CAP_MICRO_USD);
        let ledger = CostLedger::new(cap, ctx.now_ms);
        let store = OneShotStore(tokens);
        // The probe cannot know whether the caller owns the app, so the
        // conservative default (owned=false, self_id unknown) stands.
        let pricing = XPricing::default();
        match counts(ctx, &store, &ledger, &pricing) {
            Ok(snap) => snap,
            Err(e) => {
                let status = e.to_status();
                Snapshot::new(self.id(), ctx.now_ms, status, prov)
            }
        }
    }
}

/// Adapts already-loaded tokens to the TokenStore signature.
struct OneShotStore(XTokens);
impl TokenStore for OneShotStore {
    fn load(&self) -> Option<XTokens> {
        Some(self.0.clone())
    }
    fn save(&self, _tokens: &XTokens) {}
}
