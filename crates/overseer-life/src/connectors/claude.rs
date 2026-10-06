// Ported from Synara (MIT, (c) 2026 T3 Tools Inc., (c) 2026 Emanuele Di Pietro),
// apps/server/src/providerUsage/providers/claude.ts — with the CLI-delegated
// refresh removed: an expired or 401-rejected token reports `expired` /
// `needs_auth` instead of nudging `claude auth status` (R1/R6).

use std::collections::BTreeSet;
use std::path::PathBuf;

use serde_json::Value;

use crate::connector::{Connector, ConnectorInfo, Ctx, Discovered};
use crate::creds::{self, keychain, Expiry};
use crate::http::{is_auth_failure, Method, Request};
use crate::snapshot::{
    as_f64, as_str, clamp_percent, format_usd, title_case, ErrorKind, Provenance, ProvenanceKind,
    Snapshot, SourceId, Status, UsageLimit, UsageLine,
};
use crate::time::iso_to_ms;

const USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
const SESSION_WINDOW_MINS: u32 = 300;
const WEEKLY_WINDOW_MINS: u32 = 10_080;
/// Synara nudged the CLI when the token came within this buffer of expiry;
/// without refresh, expiry inside the buffer still tries the API (the
/// token is technically valid until `exp`), but past `exp` is terminal.
const LEGACY_MODEL_WEEKLY: [(&str, &str); 3] = [
    ("Fable", "seven_day_fable"),
    ("Sonnet", "seven_day_sonnet"),
    ("Opus", "seven_day_opus"),
];

struct ClaudeCreds {
    access_token: String,
    expires_at_ms: Option<i64>,
    subscription_type: Option<String>,
    rate_limit_tier: Option<String>,
    scopes: Vec<String>,
}

pub struct ClaudeConnector;

fn provenance() -> Provenance {
    Provenance {
        kind: ProvenanceKind::CliLogin,
        endpoint: Some(USAGE_URL.to_string()),
        documented: false,
    }
}

fn read_scopes(oauth: Option<&Value>) -> Vec<String> {
    let Some(oauth) = oauth else {
        return Vec::new();
    };
    if let Some(list) = oauth.get("scopes").and_then(Value::as_array) {
        return list
            .iter()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect();
    }
    as_str(&oauth["scope"])
        .map(|s| s.split_whitespace().map(str::to_string).collect())
        .unwrap_or_default()
}

fn read_claude_creds(record: Option<&Value>) -> Option<ClaudeCreds> {
    let oauth = record?.get("claudeAiOauth")?;
    let access_token = as_str(&oauth["accessToken"])?.to_string();
    Some(ClaudeCreds {
        access_token,
        expires_at_ms: as_f64(&oauth["expiresAt"]).map(|v| v as i64),
        subscription_type: as_str(&oauth["subscriptionType"]).map(str::to_string),
        rate_limit_tier: as_str(&oauth["rateLimitTier"]).map(str::to_string),
        scopes: read_scopes(Some(oauth)),
    })
}

/// Keychain service name: custom config dirs are keyed by the first eight
/// SHA-256 hex chars of their NFC spelling (Synara's derivation).
/// Returns None when strict credentials are on but no custom dir is set
/// (Synara skips the keychain entirely in that case).
fn keychain_service(ctx: &Ctx) -> Option<String> {
    use sha2::Digest;
    let secure_dir = ctx.env("CLAUDE_SECURESTORAGE_CONFIG_DIR");
    let dir = secure_dir.or_else(|| ctx.env("CLAUDE_CONFIG_DIR"));
    let strict = ctx.isolate_credentials || secure_dir.is_some();
    if strict && dir.is_none() {
        return None;
    }
    Some(match dir {
        Some(d) => {
            // macOS paths are already NFC-normalized by the filesystem layer.
            let digest = sha2::Sha256::digest(d.as_bytes());
            let hex8 = digest
                .iter()
                .take(4)
                .map(|b| format!("{b:02x}"))
                .collect::<String>();
            format!("{KEYCHAIN_SERVICE}-{hex8}")
        }
        None => KEYCHAIN_SERVICE.to_string(),
    })
}

fn claude_cred_paths(ctx: &Ctx) -> Vec<PathBuf> {
    let strict = ctx.isolate_credentials || ctx.env("CLAUDE_SECURESTORAGE_CONFIG_DIR").is_some();
    let mut paths: Vec<PathBuf> = Vec::new();
    if strict {
        let dir = match ctx.env("CLAUDE_SECURESTORAGE_CONFIG_DIR") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => ctx
                .env("CLAUDE_CONFIG_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.home(&[".claude"])),
        };
        paths.push(dir.join(".credentials.json"));
    } else if let Some(d) = ctx.env("CLAUDE_CONFIG_DIR") {
        paths.push(PathBuf::from(d).join(".credentials.json"));
    }
    if !strict {
        paths.push(ctx.home(&[".claude", ".credentials.json"]));
    }
    paths
}

fn resolve_candidates(ctx: &Ctx) -> Vec<ClaudeCreds> {
    let mut out = Vec::new();
    for path in claude_cred_paths(ctx) {
        let record = creds::read_json_file(&path);
        if let Some(c) = read_claude_creds(record.as_ref()) {
            out.push(c);
        }
    }
    let Some(service) = keychain_service(ctx) else {
        return out;
    };
    // R3: secret reads only with the explicit opt-in. Account-scoped item
    // first (Synara tries $USER, then the unscoped item).
    if !ctx.allow_keychain_secrets {
        return out;
    }
    let account = ctx
        .env("USER")
        .or_else(|| ctx.env("LOGNAME"))
        .map(str::to_string);
    let blob = keychain::read_json_secret(ctx, &service, account.as_deref())
        .or_else(|| keychain::read_json_secret(ctx, &service, None));
    if let Some(c) = read_claude_creds(blob.as_ref()) {
        out.push(c);
    }
    out
}

fn has_profile_scope(creds: &ClaudeCreds) -> bool {
    creds.scopes.is_empty() || creds.scopes.iter().any(|s| s == "user:profile")
}

fn plan_name(creds: &ClaudeCreds) -> Option<String> {
    let mut name = title_case(creds.subscription_type.as_deref()?);
    if let Some(tier) = creds.rate_limit_tier.as_deref() {
        // "..._5x_..." or "default_claude_max_5x" — first \d+x group.
        if let Some(pos) = tier.find(|c: char| c.is_ascii_digit()) {
            let tail = &tier[pos..];
            if let Some(end) = tail.find('x') {
                name.push_str(&format!(" ({})", tail[..=end].to_lowercase()));
            }
        }
    }
    Some(name)
}

/// (utilization, resets_at) from a `{utilization, resets_at}` window object.
fn window_fields(v: &Value) -> (Option<f64>, Option<&Value>) {
    if v.is_object() {
        (as_f64(&v["utilization"]), Some(&v["resets_at"]))
    } else {
        (None, None)
    }
}

fn push_limit(
    snap: &mut Snapshot,
    label: &str,
    percent: Option<f64>,
    resets: Option<&Value>,
    mins: u32,
) {
    // A negative reading is invalid, not "0 % used": drop it and mark the
    // snapshot degraded (Malformed) while keeping the windows that parsed.
    let percent = match percent {
        Some(p) if p < 0.0 => {
            if matches!(snap.status, Status::Ok) {
                snap.status = Status::Error {
                    kind: ErrorKind::Malformed,
                    message: format!("{label}: negative utilization dropped"),
                };
            }
            None
        }
        p => p,
    };
    let used_percent = clamp_percent(percent);
    let resets_at_ms = iso_to_ms(resets);
    if used_percent.is_none() && resets_at_ms.is_none() {
        return;
    }
    snap.limits.push(UsageLimit {
        name: label.to_string(),
        used_percent,
        resets_at_ms,
        window_minutes: Some(mins),
        ..Default::default()
    });
}

fn parse_usage(json: &Value, creds: &ClaudeCreds, ctx: &Ctx) -> Snapshot {
    let mut snap = Snapshot::new(
        SourceId::from("claude"),
        ctx.now_ms,
        Status::Ok,
        provenance(),
    );
    snap.account.fingerprint = Some(creds::fingerprint_of(&creds.access_token));
    snap.plan = plan_name(creds);

    let window = window_fields;

    let (p, r) = window(&json["five_hour"]);
    push_limit(&mut snap, "5h", p, r, SESSION_WINDOW_MINS);
    let (p, r) = window(&json["seven_day"]);
    push_limit(&mut snap, "Weekly", p, r, WEEKLY_WINDOW_MINS);

    // Per-model weekly windows: `limits[]` `weekly_scoped` rows named by
    // scope.model.display_name; legacy top-level seven_day_<model> fills gaps.
    let mut scoped: BTreeSet<String> = BTreeSet::new();
    if let Some(list) = json.get("limits").and_then(Value::as_array) {
        for entry in list {
            if entry["kind"].as_str() != Some("weekly_scoped") {
                continue;
            }
            let label = as_str(&entry["scope"]["model"]["display_name"])
                .map(str::to_string)
                .filter(|l| !scoped.contains(l));
            if let Some(label) = label {
                scoped.insert(label.clone());
                let percent = as_f64(&entry["percent"]);
                let resets = Some(&entry["resets_at"]);
                push_limit(&mut snap, &label, percent, resets, WEEKLY_WINDOW_MINS);
            }
        }
    }
    for (label, key) in LEGACY_MODEL_WEEKLY {
        if !scoped.contains(label) {
            let (p, r) = window(&json[key]);
            push_limit(&mut snap, label, p, r, WEEKLY_WINDOW_MINS);
        }
    }

    let extra = &json["extra_usage"];
    if extra.is_object() && extra["is_enabled"].as_bool() != Some(false) {
        if let Some(used_credits) = as_f64(&extra["used_credits"]) {
            let used_usd = format_usd(used_credits / 100.0);
            let monthly = as_f64(&extra["monthly_limit"]);
            let value = match monthly {
                Some(m) if m > 0.0 => format!("{used_usd} of {}", format_usd(m / 100.0)),
                _ => format!("{used_usd} spent"),
            };
            snap.usage_lines.push(UsageLine {
                label: "Extra usage".into(),
                value,
            });
        }
    }
    snap
}

impl Connector for ClaudeConnector {
    fn id(&self) -> SourceId {
        SourceId::from("claude")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "claude",
            name: "Claude (OAuth usage)",
            needs_network: true,
        }
    }

    fn discover(&self, ctx: &Ctx) -> Vec<Discovered> {
        let mut out = Vec::new();
        for path in claude_cred_paths(ctx) {
            out.push(creds::file_discovery(&path, "claudeAiOauth access token"));
        }
        if let Some(service) = keychain_service(ctx) {
            let account = ctx
                .env("USER")
                .or_else(|| ctx.env("LOGNAME"))
                .map(str::to_string);
            let present = keychain::item_exists(ctx, &service, account.as_deref())
                || keychain::item_exists(ctx, &service, None);
            out.push(Discovered {
                kind: "keychain",
                location: format!("keychain:{service}"),
                contains: "claudeAiOauth JSON (possibly hex-encoded)",
                present,
            });
        }
        out
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let candidates = resolve_candidates(ctx);
        if candidates.is_empty() {
            return Snapshot::needs_auth(
                self.id(),
                ctx.now_ms,
                provenance(),
                "no Claude credentials found — expected ~/.claude/.credentials.json or a keychain item",
            );
        }

        let mut inference_only: Option<Snapshot> = None;
        let mut last_error: Option<Snapshot> = None;
        let mut saw_expired = false;
        let mut saw_auth_failure = false;

        for creds in &candidates {
            if !has_profile_scope(creds) {
                let mut snap = Snapshot::new(self.id(), ctx.now_ms, Status::Ok, provenance());
                snap.account.fingerprint = Some(creds::fingerprint_of(&creds.access_token));
                snap.plan = plan_name(creds);
                inference_only = Some(snap);
                continue;
            }

            // R1: never refresh. A token past its own exp is terminal.
            if creds::expiry(creds.expires_at_ms, ctx.now_ms) == Expiry::Expired {
                saw_expired = true;
                continue;
            }

            let (url, allowed) = ctx.endpoint("CLAUDE_USAGE", USAGE_URL);
            let auth = format!("Bearer {}", creds.access_token);
            let res = ctx.http.fetch(&Request {
                url: &url,
                method: Method::Get,
                headers: &[
                    ("authorization", auth.as_str()),
                    ("accept", "application/json"),
                    ("content-type", "application/json"),
                    ("anthropic-beta", "oauth-2025-04-20"),
                    // Mirrors Synara's request shape (claude.ts:529); the
                    // version string may need updating as Claude Code
                    // ships newer releases.
                    ("user-agent", "claude-code/2.1.69"),
                ],
                body: None,
                allowed_origin: &allowed,
            });
            let res = match res {
                Ok(r) => r,
                Err(e) => {
                    last_error = Some(Snapshot::error(
                        self.id(),
                        ctx.now_ms,
                        provenance(),
                        e.error_kind(),
                        "could not reach the Claude usage endpoint".to_string(),
                    ));
                    continue;
                }
            };
            if is_auth_failure(res.status) {
                saw_auth_failure = true;
                continue;
            }
            if res.status == 429 {
                last_error = Some(Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    ErrorKind::RateLimited,
                    crate::http::rate_limit_message(&res.headers),
                ));
                continue;
            }
            if res.status != 200 {
                last_error = Some(Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    ErrorKind::Http,
                    format!("Claude usage request failed (HTTP {}).", res.status),
                ));
                continue;
            }
            match res.json {
                Some(json) => return parse_usage(&json, creds, ctx),
                None => {
                    last_error = Some(Snapshot::error(
                        self.id(),
                        ctx.now_ms,
                        provenance(),
                        ErrorKind::Malformed,
                        "Claude usage response was not valid JSON.".to_string(),
                    ));
                }
            }
        }

        if let Some(snap) = inference_only {
            return snap;
        }
        if let Some(snap) = last_error {
            return snap;
        }
        if saw_expired {
            return Snapshot::expired(self.id(), ctx.now_ms, provenance());
        }
        let hint = if saw_auth_failure {
            "stored Claude credential was rejected (401/403) — re-login required"
        } else {
            "no usable Claude credential"
        };
        Snapshot::needs_auth(self.id(), ctx.now_ms, provenance(), hint)
    }
}
