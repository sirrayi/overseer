// Ported from Synara (MIT, (c) 2026 T3 Tools Inc., (c) 2026 Emanuele Di Pietro),
// apps/server/src/providerUsage/providers/cursor.ts — SQLite access goes
// through `/usr/bin/sqlite3 -readonly` on the injectable command runner
// (no sqlite crate in this workspace), and the keychain read is gated by
// `ctx.allow_keychain_secrets` (R3).

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde_json::Value;

use crate::connector::{Connector, ConnectorInfo, Ctx, Discovered};
use crate::creds::{self, keychain};
use crate::http::{is_auth_failure, Body, Method, Request};
use crate::snapshot::{
    as_f64, clamp_percent, format_usd, title_case, ErrorKind, Metric, Provenance, ProvenanceKind,
    Snapshot, SourceId, Status, UsageLimit, UsageLine,
};
use crate::time::unix_millis_to_ms;

const USAGE_URL: &str = "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCurrentPeriodUsage";
const CREDITS_URL: &str =
    "https://api2.cursor.sh/aiserver.v1.DashboardService/GetCreditGrantsBalance";
const KEYCHAIN_SERVICE: &str = "cursor-access-token";
const ACCESS_TOKEN_KEY: &str = "cursorAuth/accessToken";
const PLAN_KEY: &str = "cursorAuth/stripeMembershipType";

struct CursorAuth {
    access_token: String,
    plan: Option<String>,
}

pub struct CursorConnector;

fn provenance() -> Provenance {
    Provenance {
        kind: ProvenanceKind::CliLogin,
        endpoint: Some(USAGE_URL.to_string()),
        documented: false,
    }
}

/// state.vscdb locations, one per platform (Synara cursorStateDbPaths).
fn state_db_paths(ctx: &Ctx) -> Vec<PathBuf> {
    let tail = ["Cursor", "User", "globalStorage", "state.vscdb"];
    match ctx.platform {
        crate::connector::Platform::MacOS => {
            let mut p = ctx.home(&["Library", "Application Support"]);
            for t in tail {
                p.push(t);
            }
            vec![p]
        }
        crate::connector::Platform::Windows => {
            let roaming = ctx
                .env("APPDATA")
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.home(&["AppData", "Roaming"]));
            let mut p = roaming;
            for t in tail {
                p.push(t);
            }
            vec![p]
        }
        _ => {
            let base = ctx
                .env("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| ctx.home(&[".config"]));
            let mut p = base;
            for t in tail {
                p.push(t);
            }
            vec![p]
        }
    }
}

/// Read `ItemTable` values through `sqlite3 -readonly -json`. R2: the
/// `-readonly` flag opens with SQLITE_OPEN_READONLY — no journal, no
/// files created, no write path. Caveat observed 2026-10-05: on a
/// WAL-mode database the existing `-shm` shared-memory index is still
/// mapped read-write by sqlite, so that file's mtime can change even
/// though the DB itself is never written. Keys are constants on argv; the
/// token comes back on stdout and never enters an error string.
fn read_item_table(
    ctx: &Ctx,
    db_path: &std::path::Path,
    keys: &[&str],
) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    let quoted = keys
        .iter()
        .map(|k| format!("'{}'", k.replace('\'', "''")))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!("SELECT key, value FROM ItemTable WHERE key IN ({quoted})");
    let out = ctx.runner.run(
        "/usr/bin/sqlite3",
        &["-readonly", "-json", &db_path.to_string_lossy(), &sql],
        5_000,
    );
    let Ok(out) = out else { return result };
    if out.status != 0 {
        return result;
    }
    let Ok(rows) = serde_json::from_slice::<Vec<Value>>(&out.stdout) else {
        return result;
    };
    for row in rows {
        if let (Some(k), Some(v)) = (row["key"].as_str(), row["value"].as_str()) {
            result.insert(k.to_string(), v.to_string());
        }
    }
    result
}

fn resolve_auth(ctx: &Ctx) -> Option<CursorAuth> {
    for db in state_db_paths(ctx) {
        if !db.is_file() {
            continue;
        }
        let values = read_item_table(ctx, &db, &[ACCESS_TOKEN_KEY, PLAN_KEY]);
        if let Some(token) = values.get(ACCESS_TOKEN_KEY).filter(|t| !t.is_empty()) {
            return Some(CursorAuth {
                access_token: token.clone(),
                plan: values.get(PLAN_KEY).cloned(),
            });
        }
    }
    if ctx.isolate_credentials {
        return None;
    }
    keychain::read_secret(ctx, KEYCHAIN_SERVICE, None).map(|token| CursorAuth {
        access_token: token,
        plan: None,
    })
}

fn cursor_headers(token: &str) -> Vec<(String, String)> {
    vec![
        ("authorization".into(), format!("Bearer {token}")),
        ("content-type".into(), "application/json".into()),
        ("accept".into(), "application/json".into()),
        ("connect-protocol-version".into(), "1".into()),
    ]
}

fn parse_usage(
    usage: &Value,
    credits: Option<&Value>,
    plan: Option<&str>,
    ctx: &Ctx,
    fp: &str,
) -> Snapshot {
    let mut snap = Snapshot::new(
        SourceId::from("cursor"),
        ctx.now_ms,
        Status::Ok,
        provenance(),
    );
    snap.account.fingerprint = Some(fp.to_string());
    snap.plan = plan.map(title_case);

    let plan_usage = &usage["planUsage"];
    let spend_limit = &usage["spendLimitUsage"];

    let total_percent = clamp_percent(as_f64(&plan_usage["totalPercentUsed"]));
    let resets_at_ms = unix_millis_to_ms(as_f64(&usage["billingCycleEnd"]));
    if total_percent.is_some() || resets_at_ms.is_some() {
        snap.limits.push(UsageLimit {
            name: "Current".into(),
            used_percent: total_percent,
            resets_at_ms,
            ..Default::default()
        });
    }

    let individual_limit = as_f64(&spend_limit["individualLimit"]);
    let individual_remaining = as_f64(&spend_limit["individualRemaining"]);
    if let Some(limit) = individual_limit.filter(|l| *l > 0.0) {
        let used = individual_remaining.map(|r| (limit - r).max(0.0));
        let value = match used {
            Some(u) => format!("{} of {}", format_usd(u / 100.0), format_usd(limit / 100.0)),
            None => format!("{} limit", format_usd(limit / 100.0)),
        };
        snap.usage_lines.push(UsageLine {
            label: "On-demand".into(),
            value,
        });
        snap.limits.push(UsageLimit {
            name: "On-demand".into(),
            used,
            limit: Some(limit),
            unit: Some("usd_cents".into()),
            ..Default::default()
        });
    }

    if let Some(credits) = credits {
        if credits["hasCreditGrants"].as_bool() != Some(false) {
            let total = as_f64(&credits["totalCents"]);
            let used = as_f64(&credits["usedCents"]);
            if let Some(total) = total.filter(|t| *t > 0.0) {
                let remaining = used.map(|u| (total - u).max(0.0)).unwrap_or(total);
                snap.usage_lines.push(UsageLine {
                    label: "Credits".into(),
                    value: format!(
                        "{} of {} remaining",
                        format_usd(remaining / 100.0),
                        format_usd(total / 100.0)
                    ),
                });
                snap.metrics.push(Metric {
                    name: "credit_grants_remaining".into(),
                    value: remaining / 100.0,
                    unit: Some("usd".into()),
                });
            }
        }
    }
    snap
}

impl Connector for CursorConnector {
    fn id(&self) -> SourceId {
        SourceId::from("cursor")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "cursor",
            name: "Cursor (DashboardService)",
            needs_network: true,
        }
    }

    fn discover(&self, ctx: &Ctx) -> Vec<Discovered> {
        let mut out: Vec<Discovered> = state_db_paths(ctx)
            .iter()
            .map(|p| creds::file_discovery(p, "ItemTable cursorAuth/* keys"))
            .collect();
        if !ctx.isolate_credentials {
            out.push(Discovered {
                kind: "keychain",
                location: format!("keychain:{KEYCHAIN_SERVICE}"),
                contains: "Cursor access token",
                present: keychain::item_exists(ctx, KEYCHAIN_SERVICE, None),
            });
        }
        out
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let Some(auth) = resolve_auth(ctx) else {
            return Snapshot::needs_auth(
                self.id(),
                ctx.now_ms,
                provenance(),
                "no Cursor access token in state.vscdb or keychain",
            );
        };
        // JWT exp is authoritative when present; expired is terminal (R1).
        if let Some(exp_ms) = creds::jwt_exp_ms(&auth.access_token) {
            if exp_ms <= ctx.now_ms {
                return Snapshot::expired(self.id(), ctx.now_ms, provenance());
            }
        }

        let (usage_url, usage_origin) = ctx.endpoint("CURSOR_USAGE", USAGE_URL);
        let headers = cursor_headers(&auth.access_token);
        let header_refs: Vec<(&str, &str)> = headers
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect();
        let res = ctx.http.fetch(&Request {
            url: &usage_url,
            method: Method::Post,
            headers: &header_refs,
            body: Some(Body::Json(serde_json::json!({}))),
            allowed_origin: &usage_origin,
        });
        let usage = match res {
            Err(e) => {
                return Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    e.error_kind(),
                    "could not reach the Cursor dashboard".to_string(),
                );
            }
            Ok(r) if is_auth_failure(r.status) => {
                return Snapshot::needs_auth(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    "stored Cursor credential was rejected (401/403) — re-login required",
                );
            }
            Ok(r) if r.status == 429 => {
                return Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    ErrorKind::RateLimited,
                    crate::http::rate_limit_message(&r.headers),
                );
            }
            Ok(r) if r.status != 200 => {
                return Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    ErrorKind::Http,
                    format!("Cursor usage request failed (HTTP {}).", r.status),
                );
            }
            Ok(r) => match r.json {
                Some(j) => j,
                None => {
                    return Snapshot::error(
                        self.id(),
                        ctx.now_ms,
                        provenance(),
                        ErrorKind::Malformed,
                        "Cursor usage response was not valid JSON.".to_string(),
                    );
                }
            },
        };

        // Credit grants are best-effort (Synara swallows their failure);
        // under the probe's one-request budget this call is skipped.
        let (credits_url, credits_origin) = ctx.endpoint("CURSOR_CREDITS", CREDITS_URL);
        let credits = ctx
            .http
            .fetch(&Request {
                url: &credits_url,
                method: Method::Post,
                headers: &header_refs,
                body: Some(Body::Json(serde_json::json!({}))),
                allowed_origin: &credits_origin,
            })
            .ok()
            .filter(|r| r.status == 200)
            .and_then(|r| r.json);

        parse_usage(
            &usage,
            credits.as_ref(),
            auth.plan.as_deref(),
            ctx,
            &creds::fingerprint_of(&auth.access_token),
        )
    }
}
