//! Provider abstraction (playbook Ch.2 §2.5, Ch.7 §2).
//! Adapters convert the canonical IR ↔ provider wire format. The hardest
//! constraint is reasoning round-tripping — solved by the opaque `Reasoning`
//! block in the IR, which providers echo back untouched.

use serde_json::Value;

use crate::ir::{Message, Usage};

pub mod anthropic;
pub mod gemini;
pub mod local;
pub mod openai;
pub mod responses;

/// Coerce a wire tool-call input into a JSON object. A non-object value
/// (string/array/number from the wire) is preserved under `_unparsed`, so
/// adapters can attach linkage fields without `IndexMut` panicking and the
/// tool sees a schema error instead of silently losing the call.
pub(crate) fn object_input(v: Value) -> Value {
    if v.is_object() {
        v
    } else {
        serde_json::json!({ "_unparsed": v })
    }
}

/// Cross-provider effort ladder (P3.2). Each adapter maps the enum onto
/// its native knob — Anthropic `thinking.budget_tokens`, OpenAI
/// `reasoning_effort`, Gemini `thinkingConfig.thinkingBudget`. A raw
/// `thinking_budget` on the request wins where the API takes tokens.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Min,
    Low,
    Medium,
    High,
    Max,
}

impl Effort {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "min" => Self::Min,
            "low" => Self::Low,
            "medium" | "med" => Self::Medium,
            "high" => Self::High,
            "max" => Self::Max,
            _ => return None,
        })
    }

    /// One notch up (adaptive effort on failure signals). Stays at Max.
    pub fn bumped(self) -> Self {
        match self {
            Self::Min => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High | Self::Max => Self::Max,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Min => "min",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Max => "max",
        }
    }
}

/// A tool definition as sent to the provider.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

/// One system-prompt segment. Segments render in order; the cache breakpoint
/// goes on the last one (stable-prefix discipline — Invariant 2).
#[derive(Debug, Clone)]
pub struct SystemSegment {
    /// Section name for the B1-10 order lint + prefix fingerprint.
    /// Structural only — adapters render `text`, never `name`, so the
    /// wire is unchanged by naming.
    pub name: &'static str,
    pub text: String,
    /// Whether this segment sits above the static/dynamic boundary.
    /// `true` → must be byte-identical across sessions (no timestamps,
    /// session IDs, git status — the documented cache killers).
    pub cacheable: bool,
}

#[derive(Debug, Clone)]
pub struct Request<'a> {
    pub model: &'a str,
    pub system: &'a [SystemSegment],
    pub tools: &'a [ToolSpec],
    pub messages: &'a [Message],
    pub max_tokens: u32,
    /// Thinking budget in tokens; None = thinking off. Wins over
    /// `effort` on APIs that take a token budget.
    pub thinking_budget: Option<u32>,
    /// Cross-provider effort knob (P3.2); adapters map it onto their
    /// native parameter. None = provider default.
    pub effort: Option<Effort>,
    /// Attach provider cache breakpoints to the prefix tail.
    pub cache_breakpoints: bool,
    /// Stable per-session prompt-cache routing key. OpenAI-family adapters
    /// send it as `prompt_cache_key` where the model profile accepts it;
    /// the other adapters ignore it.
    pub cache_key: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    PauseTurn,
    Refusal,
    /// Anthropic 4.5+: generation stopped at the window wall.
    ContextWindowExceeded,
    Other(String),
}

impl StopReason {
    pub fn as_str(&self) -> &str {
        match self {
            Self::EndTurn => "end_turn",
            Self::MaxTokens => "max_tokens",
            Self::ToolUse => "tool_use",
            Self::PauseTurn => "pause_turn",
            Self::Refusal => "refusal",
            Self::ContextWindowExceeded => "model_context_window_exceeded",
            Self::Other(s) => s.as_str(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Response {
    pub blocks: Vec<crate::ir::Block>,
    pub stop_reason: StopReason,
    pub usage: Usage,
    /// Serialized request size for the ledger's context-growth metric.
    pub request_bytes: u64,
    pub latency_ms: u64,
}

/// Failure classes per the Ch.7 §6 matrix — never collapse these into "error".
#[derive(Debug)]
pub enum ProviderError {
    RateLimit { status: u16, retry_after_ms: u64 },
    Http { status: u16, body: String },
    Transport(String),
    Malformed(String),
}

impl std::error::Error for ProviderError {}

impl std::fmt::Display for ProviderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RateLimit {
                status,
                retry_after_ms,
            } => write!(f, "rate limited ({status}); retry after {retry_after_ms}ms"),
            Self::Http { status, body } => write!(f, "{}", http_plain(*status, body)),
            Self::Transport(m) => write!(f, "transport: {m}"),
            Self::Malformed(m) => write!(f, "malformed response: {m}"),
        }
    }
}

/// The HTTP agent every adapter shares: non-2xx statuses come back as
/// values (mapped by [`send_json`]), with a 600s global timeout.
pub(crate) fn http_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(std::time::Duration::from_secs(600)))
        .build();
    ureq::Agent::new_with_config(config)
}

/// `POST url` with bearer auth and a JSON content type, then the adapter's
/// extra headers (OpenAI Chat Completions and Responses).
pub(crate) fn bearer_post(
    agent: &ureq::Agent,
    url: &str,
    api_key: &str,
    extra_headers: &[(String, String)],
) -> ureq::RequestBuilder<ureq::typestate::WithBody> {
    let mut call = agent
        .post(url)
        .header("authorization", &format!("Bearer {api_key}"))
        .header("content-type", "application/json");
    for (name, value) in extra_headers {
        call = call.header(name, value);
    }
    call
}

/// Statuses that mean "back off and retry" (529 is Anthropic's overload).
pub(crate) const RATE_LIMITED: &[u16] = &[429, 529, 503];

/// Send `body` and map the reply: a `rate_limited` status becomes
/// `RateLimit` (the `retry-after` seconds, default 5), any other non-2xx
/// becomes `Http` (rendered by `http_plain`), and a 2xx body is parsed as
/// JSON. Returns `(json, latency_ms)`, latency measured from `started`.
pub(crate) fn send_json(
    call: ureq::RequestBuilder<ureq::typestate::WithBody>,
    body: &Value,
    started: std::time::Instant,
    rate_limited: &[u16],
) -> Result<(Value, u64), ProviderError> {
    let mut resp = call
        .send_json(body)
        .map_err(|e| ProviderError::Transport(e.to_string()))?;
    let latency_ms = started.elapsed().as_millis() as u64;

    let status = resp.status().as_u16();
    let text = resp
        .body_mut()
        .read_to_string()
        .map_err(|e| ProviderError::Transport(e.to_string()))?;

    if rate_limited.contains(&status) {
        let retry_after_ms = resp
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(5)
            * 1000;
        return Err(ProviderError::RateLimit {
            status,
            retry_after_ms,
        });
    }
    if !(200..300).contains(&status) {
        return Err(ProviderError::Http { status, body: text });
    }
    let parsed =
        serde_json::from_str(&text).map_err(|e| ProviderError::Malformed(e.to_string()))?;
    Ok((parsed, latency_ms))
}

/// Render an HTTP failure as one plain-English line: a status gloss
/// plus the provider's own `error.message` when the body is the usual
/// `{"error":{…}}` envelope (Anthropic, OpenAI, and Gemini all use it);
/// non-JSON bodies fall back to a trimmed raw excerpt.
fn http_plain(status: u16, body: &str) -> String {
    let gloss = match status {
        400 => "the request was rejected",
        401 | 403 => "authentication failed — check the API key",
        404 => "model or endpoint not found",
        408 | 504 => "the request timed out",
        429 => "rate limited",
        500..=599 => "the provider had a server error",
        _ => "the request failed",
    };
    let detail = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|v| {
            v["error"]["message"]
                .as_str()
                .or_else(|| v["message"].as_str())
                .map(|s| s.trim().to_string())
        })
        .unwrap_or_else(|| body.trim().chars().take(160).collect());
    if detail.is_empty() {
        format!("{gloss} (HTTP {status})")
    } else {
        format!("{gloss} (HTTP {status}): {detail}")
    }
}

/// Providers must be safe to share with a worker thread: frontends (TUI)
/// run the agent loop off the UI thread and stream events over a channel.
pub trait Provider: Send + Sync {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError>;
    fn name(&self) -> &'static str;
}
