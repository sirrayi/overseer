//! Snapshot wire types: every connector answers one `Snapshot` whose
//! `status` carries the outcome — fetchers never panic and never throw.

use serde::{Deserialize, Serialize};

/// Stable connector identity (`"claude"`, `"x"`, `"claude-local"`, …).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SourceId(pub String);

impl From<&'static str> for SourceId {
    fn from(value: &'static str) -> Self {
        SourceId(value.to_string())
    }
}

/// Which account produced the snapshot. `label` is a display name (plan,
/// login hint); `id` is the provider's own account id (X user id — the
/// value follower endpoints are keyed on); `fingerprint` is a non-secret
/// sha256 base64url fragment of the credential — enough to tell accounts
/// apart, never enough to log in.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AccountRef {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

/// The fetch outcome. Serializes as `{"status": "ok" | "needs_auth" | …}`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Ok,
    NeedsAuth {
        hint: String,
    },
    /// A stored credential exists but its own expiry has passed. Nothing
    /// may redeem a refresh token owned by another app's CLI, so expired
    /// is a terminal state for that source this run.
    Expired,
    /// The source is reachable but cannot serve this account (e.g. a
    /// valid key with no matching subscription, or no local archive).
    Unavailable {
        reason: String,
    },
    /// A spend cap refused the request before it was sent.
    BudgetExceeded {
        cap_micro_usd: u64,
        day_spend_micro_usd: u64,
    },
    Error {
        kind: ErrorKind,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    Transport,
    Timeout,
    Http,
    /// HTTP 429; the message carries the server's Retry-After hint when
    /// it sent one.
    RateLimited,
    Malformed,
    Unsupported,
}

/// A rate/usage window. `used`/`limit` carry absolute amounts (credits,
/// USD cents, tokens) when the API reports them; `unit` names the unit
/// ("credits", "usd_cents", "percent"). Percent values are 0..=100.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct UsageLimit {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u32>,
}

/// A numeric observation (token totals, follower counts, credit balance).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Metric {
    pub name: String,
    pub value: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

/// A rendered line for values that do not fit the numeric `Metric` shape
/// ("$4.20 of $50.00", "12 recent sessions").
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UsageLine {
    pub label: String,
    pub value: String,
}

/// Where the data came from. `endpoint` is the URL fetched (never
/// carries credentials); `documented` marks official vendor APIs vs the
/// undocumented endpoints inherited from Synara's fetchers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    pub kind: ProvenanceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub documented: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceKind {
    OfficialApi,
    CliLogin,
    LocalArchive,
    BrowserSession,
    Manual,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub source: SourceId,
    #[serde(default)]
    pub account: AccountRef,
    pub fetched_at_ms: i64,
    pub status: Status,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub limits: Vec<UsageLimit>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metrics: Vec<Metric>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub usage_lines: Vec<UsageLine>,
    /// Plan/subscription name when the source reports one ("Max", "Go").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    pub provenance: Provenance,
}

impl Snapshot {
    pub fn new(source: SourceId, now_ms: i64, status: Status, provenance: Provenance) -> Self {
        Snapshot {
            source,
            account: AccountRef::default(),
            fetched_at_ms: now_ms,
            status,
            limits: Vec::new(),
            metrics: Vec::new(),
            usage_lines: Vec::new(),
            plan: None,
            provenance,
        }
    }

    pub fn needs_auth(source: SourceId, now_ms: i64, provenance: Provenance, hint: &str) -> Self {
        Snapshot::new(
            source,
            now_ms,
            Status::NeedsAuth {
                hint: hint.to_string(),
            },
            provenance,
        )
    }

    pub fn expired(source: SourceId, now_ms: i64, provenance: Provenance) -> Self {
        Snapshot::new(source, now_ms, Status::Expired, provenance)
    }

    pub fn unavailable(
        source: SourceId,
        now_ms: i64,
        provenance: Provenance,
        reason: &str,
    ) -> Self {
        Snapshot::new(
            source,
            now_ms,
            Status::Unavailable {
                reason: reason.to_string(),
            },
            provenance,
        )
    }

    pub fn error(
        source: SourceId,
        now_ms: i64,
        provenance: Provenance,
        kind: ErrorKind,
        message: String,
    ) -> Self {
        Snapshot::new(source, now_ms, Status::Error { kind, message }, provenance)
    }
}

/// Coerce to f64 for finite JSON numbers and numeric strings (several
/// provider APIs send quotas as strings). Ported from Synara
/// `providerUsage/parse.ts`.
pub fn as_f64(value: &serde_json::Value) -> Option<f64> {
    match value {
        serde_json::Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok().filter(|f| f.is_finite()),
        _ => None,
    }
}

pub fn as_non_negative(value: &serde_json::Value) -> Option<f64> {
    as_f64(value).filter(|f| *f >= 0.0)
}

pub fn as_str(value: &serde_json::Value) -> Option<&str> {
    match value {
        serde_json::Value::String(s) if !s.trim().is_empty() => Some(s.trim()),
        _ => None,
    }
}

/// Clamp into 0..=100; non-finite input is dropped.
pub fn clamp_percent(value: Option<f64>) -> Option<f64> {
    value.filter(|f| f.is_finite()).map(|f| f.clamp(0.0, 100.0))
}

/// "$4.20" — integer dollars print without trailing cents.
pub fn format_usd(amount: f64) -> String {
    if amount.fract() == 0.0 {
        format!("${amount:.0}")
    } else {
        format!("${amount:.2}")
    }
}

/// "1.2k" / "340" style compaction for token counts.
pub fn format_compact(value: f64) -> String {
    let abs = value.abs();
    if abs < 1_000.0 {
        return format!("{value:.0}");
    }
    if abs < 1_000_000.0 {
        let v = value / 1_000.0;
        return format!("{v:.1}k");
    }
    format!("{:.0}M", value / 1_000_000.0)
}

/// snake_case / kebab plan identifiers → "Pro Max"-style display names.
pub fn title_case(value: &str) -> String {
    value
        .split([' ', '_', '-'])
        .filter(|p| !p.is_empty())
        .map(|p| {
            let mut chars = p.chars();
            match chars.next() {
                Some(first) => {
                    first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase()
                }
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
