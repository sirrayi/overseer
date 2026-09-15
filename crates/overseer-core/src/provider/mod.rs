//! Provider abstraction (playbook Ch.2 §2.5, Ch.7 §2).
//! Adapters convert the canonical IR ↔ provider wire format. The hardest
//! constraint is reasoning round-tripping — solved by the opaque `Reasoning`
//! block in the IR, which providers echo back untouched.

use serde_json::Value;

use crate::ir::{Message, Usage};

pub mod anthropic;
pub mod openai;

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
    /// Thinking budget in tokens; None = thinking off.
    pub thinking_budget: Option<u32>,
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

#[derive(Debug)]
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

pub trait Provider {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError>;
    fn name(&self) -> &'static str;
}
