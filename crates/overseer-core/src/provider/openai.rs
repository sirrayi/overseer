//! OpenAI-compatible Chat Completions adapter (playbook Ch.7 §1.2).
//! Covers api.openai.com and OpenAI-compatible gateways (vLLM, Fleet,
//! Groq, Together, etc.) — verified against Fleet's vLLM fleet.
//!
//! Wire conventions handled here:
//! - tool_calls are separate `tool` role messages per result (NOT merged
//!   like Anthropic's single user turn)
//! - reasoning text comes back as `reasoning` (vLLM) or `reasoning_content`
//!   (Moonshot); chat.completions never accepts it back, so Reasoning IR
//!   blocks are preserved in the log but dropped on the wire
//! - arguments arrive as a JSON *string* inside tool_calls[].function
//! - usage: prompt_tokens includes cached; cached_tokens / reasoning_tokens
//!   in details objects (shape varies by vendor — both spellings read)
//! - finish_reason: stop|length|tool_calls|content_filter (+ raw preserved)

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{Provider, ProviderError, Request, Response, StopReason};
use crate::ir::{Block, Message, Role, Usage};

const DEFAULT_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiCompatible {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
}

impl OpenAiCompatible {
    /// `base_url` is the versioned root (…/v1); the path is appended.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build();
        OpenAiCompatible {
            agent: ureq::Agent::new_with_config(config),
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    pub fn openai(api_key: impl Into<String>) -> Self {
        Self::new(api_key, DEFAULT_URL)
    }

    fn url(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    fn build_body(req: &Request) -> Value {
        let system = req
            .system
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let mut messages = Vec::with_capacity(req.messages.len() + 1);
        if !system.is_empty() {
            messages.push(json!({"role": "system", "content": system}));
        }
        for m in req.messages {
            ir_message_to_wire(m, &mut messages);
        }

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();

        let mut body = json!({
            "model": req.model,
            "max_tokens": req.max_tokens,
            "messages": messages,
            "tools": tools,
        });
        // Effort → reasoning_effort (OpenAI-compatible gateways that
        // don't know the field ignore it — harmless passthrough).
        if let Some(e) = req.effort {
            body["reasoning_effort"] = json!(match e {
                super::Effort::Min => "minimal",
                super::Effort::Low => "low",
                super::Effort::Medium => "medium",
                // No tier above "high" exists on this API family.
                super::Effort::High | super::Effort::Max => "high",
            });
        }
        body
    }

    fn parse_response(
        body: &Value,
        request_bytes: u64,
        latency_ms: u64,
    ) -> Result<Response, ProviderError> {
        let msg = body
            .pointer("/choices/0/message")
            .ok_or_else(|| ProviderError::Malformed("missing choices[0].message".into()))?;

        let mut blocks = Vec::new();
        // Reasoning: `reasoning` (vLLM) or `reasoning_content` (Moonshot).
        // Opaque in the IR; NOT re-sent (chat.completions rejects it).
        for key in ["reasoning", "reasoning_content"] {
            if let Some(t) = msg.get(key).and_then(Value::as_str) {
                if !t.is_empty() {
                    blocks.push(Block::Reasoning {
                        raw: json!({"type": "reasoning", "text": t}),
                    });
                    break;
                }
            }
        }
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                blocks.push(Block::Text {
                    text: text.to_string(),
                });
            }
        }
        if let Some(calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let args = c
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("{}");
                let input: Value = serde_json::from_str(args).unwrap_or_else(|_| {
                    // Unparseable args are preserved raw so the tool sees a
                    // schema error rather than silently losing the call.
                    json!({"_unparsed": args})
                });
                blocks.push(Block::ToolCall {
                    id: c
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    name: c
                        .pointer("/function/name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    input,
                });
            }
        }

        let stop_reason = match body
            .pointer("/choices/0/finish_reason")
            .and_then(Value::as_str)
        {
            Some("stop") => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
            Some("content_filter") => StopReason::Refusal,
            Some(s) => StopReason::Other(s.to_string()),
            None => StopReason::Other("missing".to_string()),
        };

        let u = body.get("usage").cloned().unwrap_or(json!({}));
        let prompt = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
        let cached = u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let created = u
            .pointer("/prompt_tokens_details/created_cache_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let completion = u
            .get("completion_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let reasoning = u
            .get("reasoning_tokens")
            .or_else(|| u.pointer("/completion_tokens_details/reasoning_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let usage = Usage {
            fresh_input: prompt.saturating_sub(cached),
            cache_write: created,
            cache_read: cached,
            // completion includes reasoning; split so cost stays honest.
            output: completion.saturating_sub(reasoning),
            reasoning,
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

/// One IR message → one or more wire messages (tool results fan out into
/// individual `tool` role messages — OpenAI's pairing rule).
fn ir_message_to_wire(m: &Message, out: &mut Vec<Value>) {
    match m.role {
        Role::User => {
            for b in &m.content {
                match b {
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => out.push(json!({
                        "role": "tool",
                        "tool_call_id": tool_use_id,
                        "content": content,
                    })),
                    Block::Text { text } => out.push(json!({
                        "role": "user", "content": text
                    })),
                    _ => {}
                }
            }
        }
        Role::Assistant => {
            let mut text = String::new();
            let mut calls = Vec::new();
            for b in &m.content {
                match b {
                    Block::Text { text: t } => {
                        if !text.is_empty() {
                            text.push('\n');
                        }
                        text.push_str(t);
                    }
                    Block::ToolCall { id, name, input } => calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": serde_json::to_string(input).unwrap_or_default(),
                        }
                    })),
                    // Reasoning is never echoed back on this API family.
                    Block::Reasoning { .. } => {}
                    Block::ToolResult { .. } => {}
                }
            }
            if text.is_empty() && calls.is_empty() {
                return; // don't emit an empty assistant turn
            }
            let mut v = json!({"role": "assistant", "content": text});
            if !calls.is_empty() {
                v["tool_calls"] = json!(calls);
            }
            out.push(v);
        }
    }
}

impl Provider for OpenAiCompatible {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = Self::build_body(req);
        let request_bytes = body.to_string().len() as u64;

        let started = Instant::now();
        let mut resp = self
            .agent
            .post(&self.url())
            .header("authorization", &format!("Bearer {}", self.api_key))
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
        "openai"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{SystemSegment, ToolSpec};

    #[test]
    fn tool_result_fans_out_to_tool_messages() {
        let m = Message::tool_results(vec![
            Block::ToolResult {
                tool_use_id: "a".into(),
                content: "r1".into(),
                is_error: false,
            },
            Block::ToolResult {
                tool_use_id: "b".into(),
                content: "r2".into(),
                is_error: true,
            },
        ]);
        let mut out = Vec::new();
        ir_message_to_wire(&m, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["role"], "tool");
        assert_eq!(out[0]["tool_call_id"], "a");
        assert_eq!(out[1]["tool_call_id"], "b");
    }

    #[test]
    fn reasoning_not_resent() {
        let m = Message {
            role: Role::Assistant,
            content: vec![
                Block::Reasoning {
                    raw: json!({"type": "reasoning", "text": "hmm"}),
                },
                Block::Text {
                    text: "answer".into(),
                },
            ],
        };
        let mut out = Vec::new();
        ir_message_to_wire(&m, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["content"], "answer");
        assert!(out[0].get("reasoning").is_none());
    }

    #[test]
    fn assistant_tool_calls_shape() {
        let m = Message {
            role: Role::Assistant,
            content: vec![Block::ToolCall {
                id: "tc1".into(),
                name: "bash".into(),
                input: json!({"command": "ls"}),
            }],
        };
        let mut out = Vec::new();
        ir_message_to_wire(&m, &mut out);
        let tc = &out[0]["tool_calls"][0];
        assert_eq!(tc["type"], "function");
        assert_eq!(tc["function"]["name"], "bash");
        let args: Value =
            serde_json::from_str(tc["function"]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
    }

    #[test]
    fn parse_response_both_reasoning_fields_and_cache() {
        for key in ["reasoning", "reasoning_content"] {
            let body = json!({
                "choices": [{"message": {
                    "role": "assistant", "content": "hi", key: "thinking",
                    "tool_calls": [{"id": "t1", "type": "function",
                        "function": {"name": "f", "arguments": "{\"x\":1}"}}]
                }, "finish_reason": "tool_calls"}],
                "usage": {"prompt_tokens": 100, "completion_tokens": 50,
                    "reasoning_tokens": 20,
                    "prompt_tokens_details": {"cached_tokens": 60}}
            });
            let r = OpenAiCompatible::parse_response(&body, 10, 5).unwrap();
            assert_eq!(r.stop_reason, StopReason::ToolUse);
            assert_eq!(r.usage.fresh_input, 40);
            assert_eq!(r.usage.cache_read, 60);
            assert_eq!(r.usage.output, 30); // 50 - 20 reasoning
            assert_eq!(r.usage.reasoning, 20);
            assert!(matches!(r.blocks[0], Block::Reasoning { .. }));
        }
    }

    #[test]
    fn system_joins_segments() {
        let system = [
            SystemSegment {
                name: "test",
                text: "a".into(),
                cacheable: true,
            },
            SystemSegment {
                name: "test",
                text: "b".into(),
                cacheable: false,
            },
        ];
        let tools: Vec<ToolSpec> = vec![];
        let msgs = vec![Message::user_text("hi")];
        let req = Request {
            model: "m",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 100,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: false,
        };
        let body = OpenAiCompatible::build_body(&req);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "a\n\nb");
    }
}
