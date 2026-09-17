//! Anthropic Messages API adapter (playbook Ch.7 §1.1).
//!
//! Hard rules honored here:
//! - tool_use blocks are answered by tool_result blocks in the next single
//!   user message (the agent loop guarantees this via rehydrate assembly)
//! - thinking/redacted_thinking blocks round-trip verbatim via the opaque
//!   Reasoning IR block (filtering or rebuilding → 400)
//! - cache_control breakpoints attach to the tail of tools + system
//! - max_tokens is always required
//! - temperature is never sent (reasoning models reject it)

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{Provider, ProviderError, Request, Response, StopReason};
use crate::ir::{Block, Usage};

const API_URL: &str = "https://api.anthropic.com/v1/messages";
const API_VERSION: &str = "2023-06-01";

pub struct Anthropic {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build();
        Anthropic {
            agent: ureq::Agent::new_with_config(config),
            api_key: api_key.into(),
            base_url: API_URL.to_string(),
        }
    }

    #[cfg(test)]
    pub fn with_base_url(api_key: impl Into<String>, base: impl Into<String>) -> Self {
        let mut a = Self::new(api_key);
        a.base_url = base.into();
        a
    }

    fn build_body(req: &Request) -> Value {
        // System: segments in order; ephemeral breakpoint on the last
        // cacheable segment's tail (and on the last tool — Anthropic's
        // breakpoint hierarchy is tools → system → messages).
        let system: Vec<Value> = req
            .system
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut blk = json!({"type": "text", "text": s.text});
                if req.cache_breakpoints && i == req.system.len() - 1 {
                    blk["cache_control"] = json!({"type": "ephemeral"});
                }
                blk
            })
            .collect();

        let tools: Vec<Value> = req
            .tools
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let mut v = json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                });
                if req.cache_breakpoints && i == req.tools.len() - 1 {
                    v["cache_control"] = json!({"type": "ephemeral"});
                }
                v
            })
            .collect();

        let messages: Vec<Value> = req.messages.iter().map(ir_message_to_wire).collect();

        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "system": system,
            "tools": tools,
            "messages": messages,
        });
        // Effort → thinking budget (raw thinking_budget wins). Min =
        // thinking off entirely — no `thinking` field sent.
        if let Some(budget) = req.thinking_budget.or_else(|| {
            req.effort.and_then(|e| match e {
                super::Effort::Min => None,
                super::Effort::Low => Some(1_024),
                super::Effort::Medium => Some(4_096),
                super::Effort::High => Some(16_384),
                super::Effort::Max => Some(32_768),
            })
        }) {
            body["thinking"] = json!({"type": "enabled", "budget_tokens": budget});
        }
        body
    }

    fn parse_response(
        body: &Value,
        request_bytes: u64,
        latency_ms: u64,
    ) -> Result<Response, ProviderError> {
        let content = body
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| ProviderError::Malformed("missing content array".into()))?;

        let mut blocks = Vec::new();
        for b in content {
            match b.get("type").and_then(Value::as_str) {
                Some("text") => blocks.push(Block::Text {
                    text: b
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                }),
                // thinking / redacted_thinking → opaque blob, verbatim round-trip.
                Some("thinking") | Some("redacted_thinking") => {
                    blocks.push(Block::Reasoning { raw: b.clone() });
                }
                Some("tool_use") => blocks.push(Block::ToolCall {
                    id: b
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    name: b
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input: b.get("input").cloned().unwrap_or(json!({})),
                }),
                other => {
                    // Unknown block types are preserved, not dropped.
                    if let Some(t) = other {
                        blocks.push(Block::Reasoning {
                            raw: json!({"type": t, "raw": b.clone()}),
                        });
                    }
                }
            }
        }

        let stop_reason = match body.get("stop_reason").and_then(Value::as_str) {
            Some("end_turn") => StopReason::EndTurn,
            Some("max_tokens") => StopReason::MaxTokens,
            Some("tool_use") => StopReason::ToolUse,
            Some("pause_turn") => StopReason::PauseTurn,
            Some("refusal") => StopReason::Refusal,
            Some("model_context_window_exceeded") => StopReason::ContextWindowExceeded,
            Some(s) => StopReason::Other(s.to_string()),
            None => StopReason::Other("missing".to_string()),
        };

        let u = body.get("usage").cloned().unwrap_or(json!({}));
        let usage = Usage {
            fresh_input: u.get("input_tokens").and_then(Value::as_u64).unwrap_or(0),
            cache_write: u
                .get("cache_creation_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            cache_read: u
                .get("cache_read_input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output: u.get("output_tokens").and_then(Value::as_u64).unwrap_or(0),
            reasoning: 0, // Anthropic folds thinking into output_tokens
        };

        Ok(Response {
            blocks,
            stop_reason,
            usage,
            request_bytes,
            latency_ms,
        })
    }
}

fn ir_message_to_wire(m: &crate::ir::Message) -> Value {
    let role = match m.role {
        crate::ir::Role::User => "user",
        crate::ir::Role::Assistant => "assistant",
    };
    let content: Vec<Value> = m
        .content
        .iter()
        .map(|b| match b {
            Block::Text { text } => json!({"type": "text", "text": text}),
            Block::Reasoning { raw } => raw.clone(),
            // P7-1: screenshots ride as Anthropic image blocks (base64 source);
            // text-only test doubles never emit Image so the prefix is stable.
            Block::Image {
                media_type,
                data_b64,
                ..
            } => json!({
                "type": "image",
                "source": {"type": "base64", "media_type": media_type, "data": data_b64}
            }),
            Block::ToolCall { id, name, input } => json!({
                "type": "tool_use", "id": id, "name": name, "input": input
            }),
            Block::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => json!({
                "type": "tool_result",
                "tool_use_id": tool_use_id,
                "content": [{"type": "text", "text": content}],
                "is_error": is_error,
            }),
        })
        .collect();
    json!({"role": role, "content": content})
}

impl Provider for Anthropic {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = Self::build_body(req);
        let request_bytes = body.to_string().len() as u64;

        let started = Instant::now();
        let mut resp = self
            .agent
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json")
            .send_json(&body)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let latency_ms = started.elapsed().as_millis() as u64;

        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if status == 429 || status == 529 || status == 503 {
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

        let parsed: Value =
            serde_json::from_str(&text).map_err(|e| ProviderError::Malformed(e.to_string()))?;
        Self::parse_response(&parsed, request_bytes, latency_ms)
    }

    fn name(&self) -> &'static str {
        "anthropic"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Message, Role};
    use crate::provider::{SystemSegment, ToolSpec};

    fn sample_req<'a>(
        system: &'a [SystemSegment],
        tools: &'a [ToolSpec],
        msgs: &'a [Message],
    ) -> Request<'a> {
        Request {
            model: "claude-sonnet-5",
            system,
            tools,
            messages: msgs,
            max_tokens: 8192,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: true,
        }
    }

    #[test]
    fn cache_breakpoints_on_tails() {
        let system = vec![
            SystemSegment {
                name: "test",
                text: "static".into(),
                cacheable: true,
            },
            SystemSegment {
                name: "test",
                text: "dynamic".into(),
                cacheable: false,
            },
        ];
        let tools = vec![
            ToolSpec {
                name: "a".into(),
                description: "x".into(),
                input_schema: json!({}),
            },
            ToolSpec {
                name: "b".into(),
                description: "y".into(),
                input_schema: json!({}),
            },
        ];
        let msgs = vec![Message::user_text("hi")];
        let body = Anthropic::build_body(&sample_req(&system, &tools, &msgs));
        assert_eq!(body["tools"][1]["cache_control"]["type"], "ephemeral");
        assert!(body["tools"][0].get("cache_control").is_none());
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn reasoning_block_passthrough() {
        let thinking = json!({"type": "thinking", "thinking": "hmm", "signature": "S"});
        let msgs = [Message {
            role: Role::Assistant,
            content: vec![Block::Reasoning {
                raw: thinking.clone(),
            }],
        }];
        let wire = ir_message_to_wire(&msgs[0]);
        assert_eq!(wire["content"][0], thinking);
    }

    /// Stable-prefix lint (Invariant 2, playbook Ch.3 §9.3): the serialized
    /// tools + system prefix must be byte-identical across turns — anything
    /// volatile above the last breakpoint silently kills the cache. If this
    /// test trips, a change leaked per-request state into the static region.
    #[test]
    fn prefix_stable_across_turns() {
        let system = vec![SystemSegment {
            name: "test",
            text: "static system".into(),
            cacheable: true,
        }];
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "read a file".into(),
            input_schema: json!({"type": "object"}),
        }];
        let turn1 = vec![Message::user_text("first")];
        let turn2 = vec![
            Message::user_text("first"),
            Message {
                role: Role::Assistant,
                content: vec![Block::Text { text: "ok".into() }],
            },
            Message::user_text("second"),
        ];
        let b1 = Anthropic::build_body(&sample_req(&system, &tools, &turn1));
        let b2 = Anthropic::build_body(&sample_req(&system, &tools, &turn2));
        assert_eq!(
            serde_json::to_string(&b1["tools"]).unwrap(),
            serde_json::to_string(&b2["tools"]).unwrap()
        );
        assert_eq!(
            serde_json::to_string(&b1["system"]).unwrap(),
            serde_json::to_string(&b2["system"]).unwrap()
        );
        // Nothing volatile may appear anywhere in the prefix.
        let prefix = format!("{}{}", b1["system"], b1["tools"]);
        for volatile in ["ts_ms", "timestamp", "session_id", "uuid", "created_at"] {
            assert!(
                !prefix.contains(volatile),
                "volatile key '{volatile}' in prefix"
            );
        }
    }

    #[test]
    fn parse_tool_use_response() {
        let body = json!({
            "content": [
                {"type": "thinking", "thinking": "t", "signature": "s"},
                {"type": "tool_use", "id": "tu_1", "name": "bash", "input": {"command": "ls"}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "output_tokens": 20}
        });
        let r = Anthropic::parse_response(&body, 100, 5).unwrap();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.cache_read, 90);
        assert_eq!(r.blocks.len(), 2);
        assert!(matches!(r.blocks[1], Block::ToolCall { .. }));
    }
}
