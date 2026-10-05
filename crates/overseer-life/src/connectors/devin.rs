// Ported from Synara (MIT, (c) 2026 T3 Tools Inc., (c) 2026 Emanuele Di Pietro),
// apps/server/src/providerUsage/providers/devin.ts (+ DevinAcpSupport.ts
// credential helpers). API key from env (WINDSURF_API_KEY / DEVIN_API_KEY /
// windsurf_api_key) or the CLI's credentials.toml; POST GetUserStatus on
// server.codeium.com.

use std::path::PathBuf;

use serde_json::Value;

use crate::connector::{Connector, ConnectorInfo, Ctx, Discovered, Platform};
use crate::creds;
use crate::http::{host_of, is_auth_failure, is_loopback_host, origin_of, Body, Method, Request};
use crate::snapshot::{
    as_f64, as_non_negative, as_str, clamp_percent, format_usd, title_case, ErrorKind, Provenance,
    ProvenanceKind, Snapshot, SourceId, Status, UsageLimit, UsageLine,
};
use crate::time::{iso_to_ms, unix_seconds_to_ms};

const DEFAULT_API_SERVER_URL: &str = "https://server.codeium.com";
const GET_USER_STATUS_PATH: &str = "/exa.seat_management_pb.SeatManagementService/GetUserStatus";
const API_KEY_ENV_KEYS: [&str; 3] = ["WINDSURF_API_KEY", "DEVIN_API_KEY", "windsurf_api_key"];
const API_SERVER_ENV_KEYS: [&str; 2] = ["WINDSURF_API_SERVER_URL", "DEVIN_API_SERVER_URL"];

struct DevinAuth {
    api_key: String,
    api_server_url: String,
}

pub struct DevinConnector;

fn provenance(endpoint: &str) -> Provenance {
    Provenance {
        kind: ProvenanceKind::CliLogin,
        endpoint: Some(endpoint.to_string()),
        documented: false,
    }
}

/// The TOML store path — XDG data home, or %APPDATA% on Windows.
fn credentials_path(ctx: &Ctx) -> Option<PathBuf> {
    if ctx.platform == Platform::Windows {
        if let Some(appdata) = ctx.env("APPDATA") {
            return Some(
                PathBuf::from(appdata)
                    .join("devin")
                    .join("credentials.toml"),
            );
        }
    }
    let data_home = ctx
        .env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| ctx.home(&[".local", "share"]));
    Some(data_home.join("devin").join("credentials.toml"))
}

/// Env first, then the stored credentials.toml fields
/// `windsurf_api_key` / `api_server_url` (Synara's parse order).
fn resolve_auth(ctx: &Ctx) -> Result<Option<DevinAuth>, &'static str> {
    let mut api_key: Option<String> = None;
    for key in API_KEY_ENV_KEYS {
        if let Some(v) = ctx.env(key) {
            api_key = Some(v.to_string());
            break;
        }
    }
    let mut api_server_url: Option<String> = None;
    for key in API_SERVER_ENV_KEYS {
        if let Some(v) = ctx.env(key) {
            api_server_url = Some(v.to_string());
            break;
        }
    }
    if api_key.is_none() || api_server_url.is_none() {
        if let Some(path) = credentials_path(ctx) {
            if let Some(table) = creds::read_toml_lite(&path) {
                if api_key.is_none() {
                    api_key =
                        creds::toml_lite::find(&table, "windsurf_api_key").map(str::to_string);
                }
                if api_server_url.is_none() {
                    api_server_url =
                        creds::toml_lite::find(&table, "api_server_url").map(str::to_string);
                }
            }
        }
    }
    let Some(api_key) = api_key else {
        return Ok(None);
    };
    let raw_url = api_server_url.unwrap_or_else(|| DEFAULT_API_SERVER_URL.to_string());
    let api_server_url = validate_server_url(&raw_url)?;
    Ok(Some(DevinAuth {
        api_key,
        api_server_url,
    }))
}

/// Synara `validateDevinApiServerUrl`: http(s) only, no embedded
/// credentials, plain http only on loopback.
fn validate_server_url(raw: &str) -> Result<String, &'static str> {
    let origin = origin_of(raw).ok_or("malformed")?;
    let scheme_ok = origin.starts_with("https://") || origin.starts_with("http://");
    if !scheme_ok {
        return Err("unsupported_scheme");
    }
    if origin.starts_with("http://") {
        let loopback = host_of(raw).is_some_and(|h| is_loopback_host(&h));
        if !loopback {
            return Err("insecure_non_loopback");
        }
    }
    Ok(origin)
}

fn pick_number(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| as_f64(&v[*k]))
}

fn pick_string<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| as_str(&v[*k]))
}

fn first_record<'a>(values: &[&'a Value]) -> Option<&'a Value> {
    values.iter().copied().find(|v| v.is_object())
}

fn fmt_amount(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{v:.0}")
    } else {
        format!("{v:.2}")
    }
}

fn fmt_credits(used: Option<f64>, available: Option<f64>) -> Option<String> {
    if used.is_none() && available.is_none() {
        return None;
    }
    let used = used.unwrap_or(0.0);
    if let Some(avail) = available {
        if avail < 0.0 {
            return None;
        }
        let total = used + avail;
        if total <= 0.0 {
            return None;
        }
        return Some(format!("{} of {}", fmt_amount(used), fmt_amount(total)));
    }
    Some(format!("{} used", fmt_amount(used)))
}

fn parse_usage(json: &Value, ctx: &Ctx, fp: &str, endpoint: &str) -> Snapshot {
    let empty = Value::Null;
    let us = first_record(&[&json["userStatus"], &json["user_status"], json]).unwrap_or(&empty);
    let plan_status = first_record(&[&us["planStatus"], &us["plan_status"], us]).unwrap_or(&empty);
    let plan_info = first_record(&[
        &plan_status["planInfo"],
        &plan_status["plan_info"],
        &us["plan_info"],
    ])
    .unwrap_or(&empty);

    let mut snap = Snapshot::new(
        SourceId::from("devin"),
        ctx.now_ms,
        Status::Ok,
        provenance(endpoint),
    );
    snap.account.fingerprint = Some(fp.to_string());

    let plan_end = unix_seconds_to_ms(pick_number(plan_status, &["planEnd", "plan_end"]))
        .or_else(|| {
            unix_seconds_to_ms(pick_number(plan_info, &["planEnd", "plan_end", "end_date"]))
        })
        .or_else(|| iso_to_ms(plan_status.get("planEnd")))
        .or_else(|| iso_to_ms(plan_status.get("plan_end")))
        .or_else(|| iso_to_ms(plan_info.get("planEnd")))
        .or_else(|| iso_to_ms(plan_info.get("plan_end")))
        .or_else(|| iso_to_ms(plan_info.get("end_date")));

    let daily_used = pick_number(
        plan_status,
        &[
            "dailyQuotaRemainingPercent",
            "daily_quota_remaining_percent",
        ],
    )
    .and_then(|r| clamp_percent(Some(100.0 - r)));
    let weekly_used = pick_number(
        plan_status,
        &[
            "weeklyQuotaRemainingPercent",
            "weekly_quota_remaining_percent",
        ],
    )
    .and_then(|r| clamp_percent(Some(100.0 - r)));
    let hide_daily = plan_info["hideDailyQuota"].as_bool() == Some(true)
        || plan_info["hide_daily_quota"].as_bool() == Some(true);
    let daily_resets = unix_seconds_to_ms(pick_number(
        plan_status,
        &["dailyQuotaResetAtUnix", "daily_quota_reset_at_unix"],
    ));
    let weekly_resets = unix_seconds_to_ms(pick_number(
        plan_status,
        &["weeklyQuotaResetAtUnix", "weekly_quota_reset_at_unix"],
    ));

    if !hide_daily && (daily_used.is_some() || daily_resets.is_some()) {
        snap.limits.push(UsageLimit {
            name: "Daily".into(),
            used_percent: daily_used,
            resets_at_ms: daily_resets,
            window_minutes: Some(1_440),
            ..Default::default()
        });
    }
    let effective_weekly = weekly_used.or(if hide_daily { daily_used } else { None });
    if effective_weekly.is_some() || weekly_resets.is_some() {
        snap.limits.push(UsageLimit {
            name: "Weekly".into(),
            used_percent: effective_weekly,
            resets_at_ms: weekly_resets,
            window_minutes: Some(10_080),
            ..Default::default()
        });
    }

    if let Some(line) = fmt_credits(
        pick_nn(plan_status, &["usedPromptCredits", "used_prompt_credits"]),
        pick_number(
            plan_status,
            &["availablePromptCredits", "available_prompt_credits"],
        ),
    ) {
        snap.usage_lines.push(UsageLine {
            label: "Prompt credits".into(),
            value: line,
        });
    }
    if let Some(line) = fmt_credits(
        pick_nn(plan_status, &["usedFlexCredits", "used_flex_credits"]),
        pick_number(
            plan_status,
            &["availableFlexCredits", "available_flex_credits"],
        ),
    ) {
        snap.usage_lines.push(UsageLine {
            label: "Flex credits".into(),
            value: line,
        });
    }

    let acu_consumed = pick_nn(plan_status, &["acuConsumed", "acu_consumed"]);
    let acu_limit = pick_nn(plan_status, &["acuLimit", "acu_limit"]);
    if let Some(consumed) = acu_consumed {
        let value = match acu_limit.filter(|l| *l > 0.0) {
            Some(l) => format!("{} of {} ACU", fmt_amount(consumed), fmt_amount(l)),
            None => format!("{} ACU used", fmt_amount(consumed)),
        };
        snap.usage_lines.push(UsageLine {
            label: "ACU".into(),
            value,
        });
    } else if let Some(l) = acu_limit.filter(|l| *l > 0.0) {
        snap.usage_lines.push(UsageLine {
            label: "ACU".into(),
            value: format!("{} ACU limit", fmt_amount(l)),
        });
    }

    if let Some(micros) = pick_nn(
        plan_status,
        &["overageBalanceMicros", "overage_balance_micros"],
    ) {
        snap.usage_lines.push(UsageLine {
            label: "Extra usage balance".into(),
            value: format!("{} remaining", format_usd(micros / 1_000_000.0)),
        });
    }

    if snap.limits.is_empty() {
        if let Some(end) = plan_end {
            snap.limits.push(UsageLimit {
                name: "Current".into(),
                resets_at_ms: Some(end),
                ..Default::default()
            });
        }
    }
    snap.plan = pick_string(plan_info, &["planName", "plan_name"]).map(title_case);
    snap
}

fn pick_nn(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|k| as_non_negative(&v[*k]))
}

impl Connector for DevinConnector {
    fn id(&self) -> SourceId {
        SourceId::from("devin")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "devin",
            name: "Devin (SeatManagement GetUserStatus)",
            needs_network: true,
        }
    }

    fn discover(&self, ctx: &Ctx) -> Vec<Discovered> {
        let mut out = Vec::new();
        let env_set = API_KEY_ENV_KEYS.iter().any(|k| ctx.env(k).is_some());
        out.push(Discovered {
            kind: "env",
            location: "WINDSURF_API_KEY/DEVIN_API_KEY/windsurf_api_key".into(),
            contains: "Devin/Windsurf API key",
            present: env_set,
        });
        if let Some(path) = credentials_path(ctx) {
            out.push(creds::file_discovery(
                &path,
                "windsurf_api_key / api_server_url",
            ));
        }
        out
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let prov_default = provenance(DEFAULT_API_SERVER_URL);
        let auth = match resolve_auth(ctx) {
            Ok(Some(a)) => a,
            Ok(None) => {
                return Snapshot::needs_auth(
                    self.id(),
                    ctx.now_ms,
                    prov_default,
                    "no Devin API key — expected WINDSURF_API_KEY/DEVIN_API_KEY env or ~/.local/share/devin/credentials.toml",
                );
            }
            Err(reason) => {
                return Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    prov_default,
                    ErrorKind::Unsupported,
                    format!("Devin API server URL is invalid ({reason})."),
                );
            }
        };

        let default_url = format!("{}{}", auth.api_server_url, GET_USER_STATUS_PATH);
        let (url, allowed) = ctx.endpoint("DEVIN_USAGE", &default_url);
        let body = serde_json::json!({
            "metadata": {
                "apiKey": auth.api_key,
                "ideName": "devin",
                "ideVersion": "0.0.0",
                "extensionName": "devin",
                "extensionVersion": "0.0.0",
                "locale": "en",
            }
        });
        let authz = format!("Bearer {}", auth.api_key);
        let res = ctx.http.fetch(&Request {
            url: &url,
            method: Method::Post,
            headers: &[
                ("authorization", authz.as_str()),
                ("accept", "application/json"),
                ("content-type", "application/json"),
                ("connect-protocol-version", "1"),
            ],
            body: Some(Body::Json(body)),
            allowed_origin: &allowed,
        });
        let prov = provenance(&url);
        match res {
            Err(e) => Snapshot::error(
                self.id(),
                ctx.now_ms,
                prov,
                e.error_kind(),
                "could not reach the Devin usage endpoint".to_string(),
            ),
            Ok(r) if is_auth_failure(r.status) => Snapshot::needs_auth(
                self.id(),
                ctx.now_ms,
                prov,
                "stored Devin API key was rejected (401/403) — re-login required",
            ),
            Ok(r) if r.status == 429 => Snapshot::error(
                self.id(),
                ctx.now_ms,
                prov,
                ErrorKind::RateLimited,
                crate::http::rate_limit_message(&r.headers),
            ),
            Ok(r) if r.status != 200 => Snapshot::error(
                self.id(),
                ctx.now_ms,
                prov,
                ErrorKind::Http,
                format!("Devin usage request failed (HTTP {}).", r.status),
            ),
            Ok(r) => match r.json {
                Some(json) => parse_usage(&json, ctx, &creds::fingerprint_of(&auth.api_key), &url),
                None => Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    prov,
                    ErrorKind::Malformed,
                    "Devin usage response was not valid JSON.".to_string(),
                ),
            },
        }
    }
}
