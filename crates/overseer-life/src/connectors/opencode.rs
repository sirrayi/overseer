// Ported from Synara (MIT, (c) 2026 T3 Tools Inc., (c) 2026 Emanuele Di Pietro),
// apps/server/src/providerUsage/providers/opencode.ts +
// provider/openCodeAuthPaths.ts — auth.json lookup on the XDG paths OpenCode
// Go uses on every OS, `opencode-go` API key, GET /zen/go/v1/usage.

use std::path::PathBuf;

use serde_json::Value;

use crate::connector::{Connector, ConnectorInfo, Ctx, Discovered, Platform};
use crate::creds;
use crate::http::{Method, Request};
use crate::snapshot::{
    as_f64, as_str, clamp_percent, ErrorKind, Provenance, ProvenanceKind, Snapshot, SourceId,
    UsageLimit, UsageLine,
};
use crate::time::iso_to_ms;

const USAGE_URL: &str = "https://opencode.ai/zen/go/v1/usage";
const GO_CREDENTIAL_KEYS: [&str; 2] = ["opencode-go", "opencode"];
const WINDOWS: [(&str, &str, u32); 3] = [
    ("rolling", "5h", 300),
    ("weekly", "Weekly", 10_080),
    ("monthly", "Monthly", 43_200),
];

pub struct OpenCodeConnector;

fn provenance() -> Provenance {
    Provenance {
        kind: ProvenanceKind::CliLogin,
        endpoint: Some(USAGE_URL.to_string()),
        documented: false,
    }
}

/// Port of `resolveOpenCodeCompatibleAuthPaths` (dataDirectoryName
/// "opencode"): OPENCODE_DATA_DIR override → XDG_DATA_HOME →
/// ~/.local/share → Windows %APPDATA%/%LOCALAPPDATA% fallbacks.
fn auth_paths(ctx: &Ctx) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !paths.contains(&p) {
            paths.push(p);
        }
    };
    if let Some(dir) = ctx.env("OPENCODE_DATA_DIR") {
        push(PathBuf::from(dir).join("auth.json"));
    }
    if let Some(xdg) = ctx.env("XDG_DATA_HOME") {
        push(PathBuf::from(xdg).join("opencode").join("auth.json"));
    }
    push(ctx.home(&[".local", "share", "opencode", "auth.json"]));
    if ctx.platform == Platform::Windows {
        let roaming = ctx
            .env("APPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.home(&["AppData", "Roaming"]));
        let local = ctx
            .env("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_else(|| ctx.home(&["AppData", "Local"]));
        push(roaming.join("opencode").join("auth.json"));
        push(local.join("opencode").join("auth.json"));
    }
    paths
}

fn read_auth_record(ctx: &Ctx) -> Option<(PathBuf, Value)> {
    for path in auth_paths(ctx) {
        if let Some(record) = creds::read_json_file(&path) {
            if record.as_object().is_some_and(|o| !o.is_empty()) {
                return Some((path, record));
            }
        }
    }
    None
}

fn read_go_key(record: &Value) -> Option<String> {
    GO_CREDENTIAL_KEYS
        .iter()
        .find_map(|k| as_str(&record[*k]["key"]).map(str::to_string))
}

fn parse_usage(json: &Value, ctx: &Ctx, fp: &str) -> Snapshot {
    let usage = &json["usage"];
    let mut snap = Snapshot::new(
        SourceId::from("opencode"),
        ctx.now_ms,
        crate::snapshot::Status::Ok,
        provenance(),
    );
    snap.account.fingerprint = Some(fp.to_string());
    snap.plan = Some("Go".into());
    for (key, window, minutes) in WINDOWS {
        let entry = &usage[key];
        if !entry.is_object() {
            continue;
        }
        let used_percent = clamp_percent(as_f64(&entry["percent"]));
        let resets_at_ms = iso_to_ms(entry.get("resetsAt"));
        if used_percent.is_none() && resets_at_ms.is_none() {
            continue;
        }
        snap.limits.push(UsageLimit {
            name: window.to_string(),
            used_percent,
            resets_at_ms,
            window_minutes: Some(minutes),
            ..Default::default()
        });
    }
    snap
}

impl Connector for OpenCodeConnector {
    fn id(&self) -> SourceId {
        SourceId::from("opencode")
    }

    fn info(&self) -> ConnectorInfo {
        ConnectorInfo {
            id: "opencode",
            name: "OpenCode Go usage",
            needs_network: true,
        }
    }

    fn discover(&self, ctx: &Ctx) -> Vec<Discovered> {
        auth_paths(ctx)
            .iter()
            .map(|p| creds::file_discovery(p, "opencode-go API key"))
            .collect()
    }

    fn fetch(&self, ctx: &Ctx) -> Snapshot {
        let Some((_path, record)) = read_auth_record(ctx) else {
            return Snapshot::needs_auth(
                self.id(),
                ctx.now_ms,
                provenance(),
                "no OpenCode auth.json found — run `opencode auth login` first",
            );
        };
        let Some(go_key) = read_go_key(&record) else {
            let mut snap = Snapshot::new(
                self.id(),
                ctx.now_ms,
                crate::snapshot::Status::Ok,
                provenance(),
            );
            snap.usage_lines.push(UsageLine {
                label: "Limits".into(),
                value: "OpenCode is signed in locally. Live 5h / weekly / monthly bars need an OpenCode Go login (`opencode auth login`).".into(),
            });
            return snap;
        };

        let (url, allowed) = ctx.endpoint("OPENCODE_USAGE", USAGE_URL);
        let authz = format!("Bearer {go_key}");
        let res = ctx.http.fetch(&Request {
            url: &url,
            method: Method::Get,
            headers: &[("authorization", authz.as_str())],
            body: None,
            allowed_origin: &allowed,
        });
        match res {
            Err(e) => Snapshot::error(
                self.id(),
                ctx.now_ms,
                provenance(),
                e.error_kind(),
                "could not reach the OpenCode usage API".to_string(),
            ),
            Ok(r) if r.status == 401 => Snapshot::needs_auth(
                self.id(),
                ctx.now_ms,
                provenance(),
                "stored OpenCode Go key was rejected (401) — re-login required",
            ),
            Ok(r) if r.status == 403 => Snapshot::unavailable(
                self.id(),
                ctx.now_ms,
                provenance(),
                "this OpenCode Go key is valid but has no active Go subscription",
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
                format!("OpenCode usage request failed (HTTP {}).", r.status),
            ),
            Ok(r) => match r.json {
                Some(json) => {
                    let snap = parse_usage(&json, ctx, &creds::fingerprint_of(&go_key));
                    if snap.limits.is_empty() {
                        Snapshot::error(
                            self.id(),
                            ctx.now_ms,
                            provenance(),
                            ErrorKind::Malformed,
                            "OpenCode usage response contained no usage windows.".into(),
                        )
                    } else {
                        snap
                    }
                }
                None => Snapshot::error(
                    self.id(),
                    ctx.now_ms,
                    provenance(),
                    ErrorKind::Malformed,
                    "OpenCode usage response was not valid JSON.".into(),
                ),
            },
        }
    }
}
