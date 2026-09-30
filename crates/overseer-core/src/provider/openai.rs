//! OpenAI-compatible Chat Completions adapter (playbook Ch.7 §1.2).
//! Covers api.openai.com and OpenAI-compatible gateways (vLLM, Groq,
//! Together, etc.) — verified against hosted vLLM gateways.
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

use std::time::Instant;

use serde_json::{json, Value};

use super::{Provider, ProviderError, Request, Response, StopReason};
use crate::ir::{Block, Message, Role, Usage};

const DEFAULT_URL: &str = "https://api.openai.com/v1";

pub struct OpenAiCompatible {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
    /// Extra request headers (e.g. opencode Go's `x-opencode-session`,
    /// required for routing on `opencode.ai/zen/go`). Applied after the
    /// standard auth/content-type headers on every call.
    extra_headers: Vec<(String, String)>,
}

impl OpenAiCompatible {
    /// `base_url` is the versioned root (…/v1); the path is appended.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        OpenAiCompatible {
            agent: super::http_agent(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            extra_headers: Vec::new(),
        }
    }

    /// Attach an extra header sent on every request (endpoint-required
    /// routing headers like `x-opencode-session`).
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
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
        // Chat Completions only knows `type:"function"` tool calls: the
        // computer tool is a plain function on this wire. Responses-API CU
        // linkage (`previous_response_id`, safety checks) lives in
        // responses.rs. Section order system/messages/tools stays frozen.
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
        if let Some(k) = &req.cache_key {
            body["prompt_cache_key"] = json!(k);
        }
        // Param filter (Ch.7 §2.2): the conservative gateway profile
        // rejects reasoning_effort, so it is stripped there while
        // unknown/OpenAI models keep it. Core keys are never stripped.
        crate::profile::strip_optional_params(
            &mut body,
            req.model,
            &[
                "temperature",
                "top_p",
                "frequency_penalty",
                "presence_penalty",
                "reasoning_effort",
                "prompt_cache_key",
            ],
        );
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
                // Unparseable (or non-object) args are preserved raw so the
                // tool sees a schema error rather than silently losing the
                // call.
                let input: Value = serde_json::from_str(args)
                    .map(super::object_input)
                    .unwrap_or_else(|_| json!({"_unparsed": args}));
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
        // OpenAI: prompt_tokens_details.cached_tokens. DeepSeek:
        // prompt_cache_hit_tokens / prompt_cache_miss_tokens (the miss
        // count falls back to prompt − hit). OpenAI's field wins.
        let hit = u.get("prompt_cache_hit_tokens").and_then(Value::as_u64);
        let (fresh, cached) = match (
            u.pointer("/prompt_tokens_details/cached_tokens")
                .and_then(Value::as_u64),
            hit,
        ) {
            (Some(c), _) => (prompt.saturating_sub(c), c),
            (None, Some(h)) => (
                u.get("prompt_cache_miss_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| prompt.saturating_sub(h)),
                h,
            ),
            (None, None) => (prompt, 0),
        };
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
            fresh_input: fresh,
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

/// P7-2 CU ack gate: a computer input with non-empty
/// `pending_safety_checks` and no `safety_ack: true` must NOT dispatch —
/// the engine holds the turn until the checks are acknowledged.
pub fn safety_ack_complete(input: &Value) -> bool {
    let pending = input
        .get("pending_safety_checks")
        .and_then(Value::as_array)
        .map(|a| !a.is_empty())
        .unwrap_or(false);
    if !pending {
        return true;
    }
    input
        .get("safety_ack")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// One IR message → one or more wire messages (tool results fan out into
/// individual `tool` role messages — OpenAI's pairing rule). Every tool
/// call — `computer` included — is a `type:"function"` call here.
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
                    // P7-1: screenshots ride as image_url blocks; other
                    // variants (Reasoning/ToolCall) never appear user-side.
                    Block::Text { text } => out.push(json!({
                        "role": "user", "content": text
                    })),
                    Block::Image {
                        media_type,
                        data_b64,
                        ..
                    } => out.push(json!({
                        "role": "user",
                        "content": [{
                            "type": "image_url",
                            "image_url": {"url": format!("data:{media_type};base64,{data_b64}")}
                        }]
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
                    // Screenshots never appear assistant-side; skip.
                    Block::Image { .. } => {}
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
        // P7-2 fail-closed: a CU request without credentials never reaches
        // the wire — honest error, never a silent skip.
        if req.tools.iter().any(|t| t.name == "computer") && self.api_key.trim().is_empty() {
            return Err(ProviderError::Transport(
                "openai computer-use: no API key — refusing to send CU request".into(),
            ));
        }

        let started = Instant::now();
        let call = super::bearer_post(&self.agent, &self.url(), &self.api_key, &self.extra_headers);
        let (parsed, latency_ms) = super::send_json(call, &body, started, super::RATE_LIMITED)?;
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
            cache_key: None,
        };
        let body = OpenAiCompatible::build_body(&req);
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "a\n\nb");
    }
    /// `with_header` extras must reach the wire — opencode Go 400s without
    /// `x-opencode-session`, so this proves the header is really sent
    /// (a body-only test would miss a dropped header).
    #[test]
    fn extra_headers_reach_the_wire() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut head = Vec::new();
            let mut buf = [0u8; 4096];
            // Read until end of headers — the body may still be in flight.
            while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                let n = sock.read(&mut buf).unwrap();
                assert!(n > 0, "connection closed before headers complete");
                head.extend_from_slice(&buf[..n]);
            }
            let body = r#"{"choices":[{"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#;
            write!(
                sock,
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .unwrap();
            String::from_utf8_lossy(&head).to_string()
        });

        let system: Vec<SystemSegment> = vec![];
        let tools: Vec<ToolSpec> = vec![];
        let msgs = vec![Message::user_text("hi")];
        let req = Request {
            model: "m",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 10,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: false,
            cache_key: None,
        };
        let provider = OpenAiCompatible::new("k", format!("http://127.0.0.1:{port}/v1"))
            .with_header("x-opencode-session", "overseer-test");
        let resp = provider.complete(&req).unwrap();
        assert_eq!(resp.stop_reason, StopReason::EndTurn);
        let head = server.join().unwrap();
        assert!(
            head.to_ascii_lowercase()
                .contains("x-opencode-session: overseer-test"),
            "session header missing from request head: {head}"
        );
    }

    /// K4: official OpenAI profiles carry `prompt_cache_key`; the
    /// conservative gateway profile (opencode / vLLM) never does.
    #[test]
    fn prompt_cache_key_only_on_openai_profiles() {
        let system: Vec<SystemSegment> = vec![];
        let tools: Vec<ToolSpec> = vec![];
        let msgs = vec![Message::user_text("hi")];
        let body_for = |model| {
            OpenAiCompatible::build_body(&Request {
                model,
                system: &system,
                tools: &tools,
                messages: &msgs,
                max_tokens: 100,
                thinking_budget: None,
                effort: None,
                cache_breakpoints: false,
                cache_key: Some("sess-1".into()),
            })
        };
        assert_eq!(body_for("gpt-5.5")["prompt_cache_key"], "sess-1");
        assert!(body_for("deepseek-v4.1-flash")
            .get("prompt_cache_key")
            .is_none());
    }

    #[test]
    fn param_filter_strips_reasoning_effort_on_gateway() {
        // The conservative gateway profile rejects reasoning_effort: effort
        // maps then strips, so vLLM never sees a gateway-specific knob.
        let system: Vec<SystemSegment> = vec![];
        let tools: Vec<ToolSpec> = vec![];
        let msgs = vec![Message::user_text("hi")];
        let req = Request {
            model: "deepseek-v4.1-flash",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 100,
            thinking_budget: None,
            effort: Some(super::super::Effort::High),
            cache_breakpoints: false,
            cache_key: None,
        };
        let body = OpenAiCompatible::build_body(&req);
        assert!(body.get("reasoning_effort").is_none());
        for core in ["model", "messages", "tools", "max_tokens"] {
            assert!(body.get(core).is_some(), "{core} stripped");
        }
    }

    #[test]
    fn build_body_byte_stable_when_all_supported() {
        // Unknown model → FALLBACK union: reasoning_effort survives, and a
        // re-strip of the supported set is a byte-identical no-op.
        let system: Vec<SystemSegment> = vec![];
        let tools: Vec<ToolSpec> = vec![];
        let msgs = vec![Message::user_text("hi")];
        let req = Request {
            model: "some-future-model",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 100,
            thinking_budget: None,
            effort: Some(super::super::Effort::High),
            cache_breakpoints: false,
            cache_key: None,
        };
        let b1 = OpenAiCompatible::build_body(&req);
        assert_eq!(b1["reasoning_effort"], "high");
        let mut b2 = b1.clone();
        crate::profile::strip_optional_params(
            &mut b2,
            "some-future-model",
            &[
                "temperature",
                "top_p",
                "frequency_penalty",
                "presence_penalty",
                "reasoning_effort",
            ],
        );
        assert_eq!(
            serde_json::to_string(&b1).unwrap(),
            serde_json::to_string(&b2).unwrap()
        );
    }

    #[test]
    fn unacked_safety_check_blocks_ack_gate() {
        // P7-2 ack gate: pending checks without safety_ack → blocked.
        assert!(!safety_ack_complete(
            &json!({"action": "click", "pending_safety_checks": [{"id": "s1"}]})
        ));
        assert!(safety_ack_complete(
            &json!({"action": "click", "pending_safety_checks": [{"id": "s1"}], "safety_ack": true})
        ));
        assert!(safety_ack_complete(&json!({"action": "click"})));
        assert!(safety_ack_complete(
            &json!({"action": "click", "pending_safety_checks": []})
        ));
    }

    #[test]
    fn unconfigured_cu_request_errors_honestly() {
        // P7-2 fail-closed: empty key + computer tool → Transport, no wire.
        let p = OpenAiCompatible::new("", "http://127.0.0.1:9/v1");
        let tools = vec![ToolSpec {
            name: "computer".into(),
            description: "cu".into(),
            input_schema: json!({"type": "object"}),
        }];
        let msgs = vec![Message::user_text("hi")];
        let system: Vec<SystemSegment> = vec![];
        let req = Request {
            model: "m",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 10,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: false,
            cache_key: None,
        };
        let err = p.complete(&req).unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)));
    }

    /// C12: a Chat Completions body only ever carries `type:"function"`
    /// tool calls — the computer tool is a plain function here, and a
    /// result is never re-typed by sniffing its content.
    #[test]
    fn chat_body_only_emits_function_tool_calls() {
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    Block::ToolCall {
                        id: "cu_1".into(),
                        name: "computer".into(),
                        input: json!({"action": "click", "previous_response_id": "r"}),
                    },
                    Block::ToolCall {
                        id: "b_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "ls"}),
                    },
                ],
            },
            Message::tool_results(vec![
                Block::ToolResult {
                    tool_use_id: "cu_1".into(),
                    content: "clicked".into(),
                    is_error: false,
                },
                Block::ToolResult {
                    tool_use_id: "b_1".into(),
                    content: "[computer] looks like CU".into(),
                    is_error: false,
                },
            ]),
        ];
        let system: Vec<SystemSegment> = vec![];
        let tools: Vec<ToolSpec> = vec![];
        let req = Request {
            model: "gpt-5.5",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 100,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: false,
            cache_key: None,
        };
        let body = OpenAiCompatible::build_body(&req);
        let wire = body["messages"].as_array().unwrap();
        let calls: Vec<&Value> = wire
            .iter()
            .filter_map(|m| m.get("tool_calls").and_then(Value::as_array))
            .flatten()
            .collect();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|c| c["type"] == "function"), "{calls:?}");
        assert_eq!(calls[0]["function"]["name"], "computer");
        assert!(
            wire.iter().all(|m| m.get("type").is_none()),
            "chat tool messages carry no type marker: {wire:?}"
        );
    }

    /// C3: a chat tool call whose arguments are a non-object JSON value
    /// is preserved raw and never panics.
    #[test]
    fn non_object_arguments_preserved() {
        for args in ["\"a string\"", "[1,2]"] {
            let body = json!({
                "choices": [{"message": {"role": "assistant", "content": "",
                    "tool_calls": [{"id": "c", "type": "function",
                        "function": {"name": "write", "arguments": args}}]},
                    "finish_reason": "tool_calls"}]
            });
            let r = OpenAiCompatible::parse_response(&body, 1, 1).unwrap();
            match &r.blocks[0] {
                Block::ToolCall { input, .. } => {
                    let raw: Value = serde_json::from_str(args).unwrap();
                    assert_eq!(input["_unparsed"], raw);
                }
                other => panic!("expected ToolCall, got {other:?}"),
            }
        }
    }

    /// K2: DeepSeek reports cache hits as `prompt_cache_hit_tokens` /
    /// `prompt_cache_miss_tokens` (no prompt_tokens_details).
    #[test]
    fn deepseek_usage_shape() {
        let body = |usage: Value| {
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"}], "usage": usage})
        };
        let r = OpenAiCompatible::parse_response(
            &body(json!({"prompt_tokens": 100, "completion_tokens": 7,
                "prompt_cache_hit_tokens": 64, "prompt_cache_miss_tokens": 36})),
            1,
            1,
        )
        .unwrap();
        assert_eq!(r.usage.cache_read, 64);
        assert_eq!(r.usage.fresh_input, 36);
        // Miss count absent → prompt − hit.
        let r = OpenAiCompatible::parse_response(
            &body(json!({"prompt_tokens": 100, "completion_tokens": 7,
                "prompt_cache_hit_tokens": 60})),
            1,
            1,
        )
        .unwrap();
        assert_eq!(r.usage.cache_read, 60);
        assert_eq!(r.usage.fresh_input, 40);
        // OpenAI's own field wins when both are present.
        let r = OpenAiCompatible::parse_response(
            &body(json!({"prompt_tokens": 100, "completion_tokens": 7,
                "prompt_tokens_details": {"cached_tokens": 10},
                "prompt_cache_hit_tokens": 60})),
            1,
            1,
        )
        .unwrap();
        assert_eq!(r.usage.cache_read, 10);
        assert_eq!(r.usage.fresh_input, 90);
    }
}
