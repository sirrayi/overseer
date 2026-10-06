// Ported from Synara (MIT, (c) 2026 T3 Tools Inc., (c) 2026 Emanuele Di Pietro),
// apps/server/src/providerUsage/providers/codex.ts — with the refresh-token
// redemption removed entirely: stale credentials report `expired`, never
// redeem at auth.openai.com (R1). Banked-reset credits ride the `codex`
// app-server in Synara; that spawn is dropped here (R6).

use std::path::PathBuf;

use serde_json::Value;

use crate::connector::{Connector, ConnectorInfo, Ctx, Discovered};
use crate::creds::{self, keychain, Expiry};
use crate::http::{is_auth_failure, Method, Request};
use crate::snapshot::{
    as_f64, as_str, clamp_percent, format_usd, title_case, ErrorKind, Metric, Provenance,
    ProvenanceKind, Snapshot, SourceId, Status, UsageLimit, UsageLine,
};
use crate::time::{iso_to_ms, unix_seconds_to_ms};

const USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";
const KEYCHAIN_SERVICE: &str = "Codex Auth";
/// The CLI rotates the login when the JWT is within 5 min of exp; we keep
/// the number only to order "expired" vs "still usable" — no refresh.
const LAST_REFRESH_MAX_AGE_MS: i64 = 8 * 24 * 60 * 60 * 1000;
/// Credits are counted, not dollars; $0.04 each is OpenAI's list price.
const CREDIT_LIST_PRICE_USD: f64 = 0.04;
/// Ten years: a longer `reset_after_seconds` is treated as unknown.
const MAX_RESET_AFTER_S: f64 = 10.0 * 365.0 * 86_400.0;

struct CodexAuth {
    access_token: String,
    account_id: Option<String>,
    /// `last_refresh` field from auth.json (RFC 3339) — staleness fallback
    /// when the access token carries no JWT exp.
    last_refresh_ms: Option<i64>,
}

enum Resolved {
    Oauth(CodexAuth),
    ApiKeyOnly,
}

pub struct CodexConnector;

fn provenance() -> Provenance {
    Provenance {
        kind: ProvenanceKind::CliLogin,
        endpoint: Some(USAGE_URL.to_string()),
        documented: false,
    }
}

fn auth_file_paths(ctx: &Ctx) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !paths.contains(&p) {
            paths.push(p);
        }
    };
    if let Some(home) = ctx.env("CODEX_HOME") {
        push(PathBuf::from(home).join("auth.json"));
    }
    if ctx.isolate_credentials {
        return paths;
    }
    if let Some(xdg) = ctx.env("XDG_CONFIG_HOME") {
        push(PathBuf::from(xdg).join("codex").join("auth.json"));
    }
    push(ctx.home(&[".config", "codex", "auth.json"]));
    push(ctx.home(&[".codex", "auth.json"]));
    paths
}

fn read_auth_record(record: Option<&Value>) -> Option<Resolved> {
    let record = record?;
    let tokens = record.get("tokens");
    if let Some(access_token) = tokens
        .and_then(|t| as_str(&t["access_token"]))
        .map(str::to_string)
    {
        return Some(Resolved::Oauth(CodexAuth {
            access_token,
            account_id: tokens
                .and_then(|t| as_str(&t["account_id"]))
                .map(str::to_string),
            last_refresh_ms: iso_to_ms(record.get("last_refresh")),
        }));
    }
    if as_str(&record["OPENAI_API_KEY"]).is_some() {
        return Some(Resolved::ApiKeyOnly);
    }
    None
}

fn resolve_auth(ctx: &Ctx) -> Option<Resolved> {
    let mut saw_api_key_only = false;
    for path in auth_file_paths(ctx) {
        let record = creds::read_json_file(&path);
        match read_auth_record(record.as_ref()) {
            Some(Resolved::Oauth(auth)) => return Some(Resolved::Oauth(auth)),
            Some(Resolved::ApiKeyOnly) => saw_api_key_only = true,
            None => {}
        }
    }
    if !ctx.isolate_credentials && ctx.allow_keychain_secrets {
        let blob = keychain::read_json_secret(ctx, KEYCHAIN_SERVICE, None);
        match read_auth_record(blob.as_ref()) {
            Some(Resolved::Oauth(auth)) => return Some(Resolved::Oauth(auth)),
            Some(Resolved::ApiKeyOnly) => saw_api_key_only = true,
            None => {}
        }
    }
    if saw_api_key_only {
        Some(Resolved::ApiKeyOnly)
    } else {
        None
    }
}

/// Token staleness, Synara's rule minus the refresh it used to trigger:
/// JWT exp wins; `last_refresh` older than 8 days is the fallback.
fn stale_reason(auth: &CodexAuth, now_ms: i64) -> Expiry {
    if let Some(exp_ms) = creds::jwt_exp_ms(&auth.access_token) {
        return creds::expiry(Some(exp_ms), now_ms);
    }
    match auth.last_refresh_ms {
        Some(ms) if now_ms - ms > LAST_REFRESH_MAX_AGE_MS => Expiry::Expired,
        _ => Expiry::Unknown,
    }
}

fn parse_usage(
    json: &Value,
    headers: &[(String, String)],
    ctx: &Ctx,
    auth: &CodexAuth,
) -> Snapshot {
    let mut snap = Snapshot::new(
        SourceId::from("codex"),
        ctx.now_ms,
        Status::Ok,
        provenance(),
    );
    snap.account.fingerprint = Some(creds::fingerprint_of(&auth.access_token));

    let header = |name: &str| -> Option<f64> {
        headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .and_then(|(_, v)| v.trim().parse::<f64>().ok())
    };

    let rate_limit = &json["rate_limit"];
    let mut push_window = |label: &str, window: &Value, hdr: &str, fallback_mins: u32| {
        if !window.is_object() {
            return;
        }
        let used_percent =
            clamp_percent(header(hdr)).or_else(|| clamp_percent(as_f64(&window["used_percent"])));
        let resets_at_ms = unix_seconds_to_ms(as_f64(&window["reset_at"])).or_else(|| {
            // Upstream data: anything past ten years is nonsense, not a reset.
            as_f64(&window["reset_after_seconds"])
                .filter(|s| *s > 0.0 && *s <= MAX_RESET_AFTER_S)
                .and_then(|s| ctx.now_ms.checked_add((s * 1000.0) as i64))
        });
        let minutes = as_f64(&window["limit_window_seconds"])
            .map(|s| (s / 60.0).round() as u32)
            .unwrap_or(fallback_mins);
        if used_percent.is_none() && resets_at_ms.is_none() {
            return;
        }
        snap.limits.push(UsageLimit {
            name: label.to_string(),
            used_percent,
            resets_at_ms,
            window_minutes: Some(minutes),
            ..Default::default()
        });
    };
    push_window(
        "5h",
        &rate_limit["primary_window"],
        "x-codex-primary-used-percent",
        300,
    );
    push_window(
        "Weekly",
        &rate_limit["secondary_window"],
        "x-codex-secondary-used-percent",
        10_080,
    );

    let credits = &json["credits"];
    let balance = header("x-codex-credits-balance").or_else(|| as_f64(&credits["balance"]));
    if let Some(balance) = balance {
        if credits["has_credits"].as_bool() != Some(false) || balance > 0.0 {
            snap.usage_lines.push(UsageLine {
                label: "Credits".into(),
                value: format!(
                    "{} remaining (≈ {})",
                    format_credits(balance),
                    format_usd(balance * CREDIT_LIST_PRICE_USD)
                ),
            });
            snap.metrics.push(Metric {
                name: "credits".into(),
                value: balance,
                unit: Some("credits".into()),
            });
        }
    }
    snap.plan = as_str(&json["plan_type"]).map(title_case);
    snap
}

fn format_credits(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

impl Connector for CodexConnector {
    fn id(&self) -> SourceId {
        SourceId::from("codex")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "codex",
            name: "Codex (ChatGPT backend usage)",
            needs_network: true,
        }
    }

    fn discover(&self, ctx: &Ctx) -> Vec<Discovered> {
        let mut out: Vec<Discovered> = auth_file_paths(ctx)
            .iter()
            .map(|p| creds::file_discovery(p, "tokens.access_token OAuth"))
            .collect();
        if !ctx.isolate_credentials {
            out.push(Discovered {
                kind: "keychain",
                location: format!("keychain:{KEYCHAIN_SERVICE}"),
                contains: "auth.json-equivalent JSON",
                present: keychain::item_exists(ctx, KEYCHAIN_SERVICE, None),
            });
        }
        out
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let auth = match resolve_auth(ctx) {
            Some(Resolved::Oauth(a)) => a,
            Some(Resolved::ApiKeyOnly) => {
                return Snapshot::unavailable(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    "Codex API-key auth has no usage endpoint — sign in with ChatGPT to see usage",
                );
            }
            None => {
                return Snapshot::needs_auth(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    "no Codex auth.json found — run `codex` sign-in first",
                );
            }
        };

        // R1: never redeem a refresh token. Stale → expired, terminal.
        if let Expiry::Expired = stale_reason(&auth, ctx.now_ms) {
            return Snapshot::expired(self.id(), ctx.now_ms, provenance());
        }

        let (url, allowed) = ctx.endpoint("CODEX_USAGE", USAGE_URL);
        let authz = format!("Bearer {}", auth.access_token);
        let mut headers: Vec<(&str, &str)> = vec![
            ("authorization", authz.as_str()),
            ("accept", "application/json"),
            // UA kept to match the ported request shape.
            ("user-agent", "Synara"),
        ];
        let account_id = auth.account_id.clone();
        if let Some(id) = account_id.as_deref() {
            headers.push(("chatgpt-account-id", id));
        }
        let res = ctx.http.fetch(&Request {
            url: &url,
            method: Method::Get,
            headers: &headers,
            body: None,
            allowed_origin: &allowed,
        });
        match res {
            Err(e) => Snapshot::error(
                self.id(),
                ctx.now_ms,
                provenance(),
                e.error_kind(),
                "could not reach the Codex usage endpoint".to_string(),
            ),
            Ok(r) if is_auth_failure(r.status) => Snapshot::needs_auth(
                self.id(),
                ctx.now_ms,
                provenance(),
                "stored Codex credential was rejected (401/403) — re-login required",
            ),
            Ok(r) if r.status == 429 => Snapshot::error(
                self.id(),
                ctx.now_ms,
                provenance(),
                ErrorKind::RateLimited,
                crate::http::rate_limit_message(&r.headers),
            ),
            Ok(r) if r.status != 200 => Snapshot::error(
                self.id(),
                ctx.now_ms,
                provenance(),
                ErrorKind::Http,
                format!("Codex usage request failed (HTTP {}).", r.status),
            ),
            Ok(r) => match r.json {
                Some(json) => parse_usage(&json, &r.headers, ctx, &auth),
                None => Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    ErrorKind::Malformed,
                    "Codex usage response was not valid JSON.".to_string(),
                ),
            },
        }
    }
}
