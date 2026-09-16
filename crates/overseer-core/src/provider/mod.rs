//! Provider abstraction (playbook Ch.2 §2.5, Ch.7 §2).
//! Adapters convert the canonical IR ↔ provider wire format. The hardest
//! constraint is reasoning round-tripping — solved by the opaque `Reasoning`
//! block in the IR, which providers echo back untouched.

use serde_json::Value;

use crate::ir::{Message, Usage};

pub mod anthropic;
pub mod gemini;
pub mod openai;

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
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("rate limited ({status}); retry after {retry_after_ms}ms")]
    RateLimit { status: u16, retry_after_ms: u64 },
    #[error("HTTP {status}: {body}")]
    Http { status: u16, body: String },
    #[error("transport: {0}")]
    Transport(String),
    #[error("malformed response: {0}")]
    Malformed(String),
}

/// Providers must be safe to share with a worker thread: frontends (TUI)
/// run the agent loop off the UI thread and stream events over a channel.
pub trait Provider: Send + Sync {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError>;
    fn name(&self) -> &'static str;
}
