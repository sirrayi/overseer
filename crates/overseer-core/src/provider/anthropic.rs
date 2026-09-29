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

/// P7-2 hosted computer-use toolset version (Anthropic CU contract id).
/// Sent as the `anthropic-beta` header only when the request advertises a
/// `computer` tool — text-only requests send no beta header (prefix stable).
pub const COMPUTER_TOOLSET_BETA: &str = "computer_toolset_20260801";

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
        // System: segments in order; ephemeral breakpoint on the LAST
        // CACHEABLE segment (a trailing volatile segment must never carry
        // it — invariant 2), and on the last tool (Anthropic's breakpoint
        // hierarchy is tools → system → messages).
        let last_cacheable = req.system.iter().rposition(|s| s.cacheable);
        let system: Vec<Value> = req
            .system
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut blk = json!({"type": "text", "text": s.text});
                if req.cache_breakpoints && Some(i) == last_cacheable {
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
        // Param filter (Ch.7 §2.2): drop optional keys the profile rejects
        // so a cross-family knob never 400s the request. Only emitted
        // optionals are named; core keys (model/messages/tools/max_tokens)
        // are never stripped (supports_param refuses them anyway).
        crate::profile::strip_optional_params(
            &mut body,
            req.model,
            &[
                "temperature",
                "top_p",
                "top_k",
                "stop_sequences",
                "thinking",
            ],
        );
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
                // P7-2 CU: batched tool_use (incl. computer actions) each
                // becomes one ToolCall; the gate's classify() holds on the
                // decoded input (screenshot→Read … cred focus→Identity).
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
                // P7-2 CU image result: base64 source → Image block with
                // zeroed scaling metadata (the tool fills real dims).
                Some("image") => {
                    let (media_type, data_b64) = b
                        .get("source")
                        .map(|s| {
                            (
                                s.get("media_type")
                                    .and_then(Value::as_str)
                                    .unwrap_or("image/png"),
                                s.get("data").and_then(Value::as_str).unwrap_or(""),
                            )
                        })
                        .unwrap_or(("image/png", ""));
                    blocks.push(Block::Image {
                        media_type: media_type.to_string(),
                        data_b64: data_b64.to_string(),
                        px_w: 0,
                        px_h: 0,
                        sent_w: 0,
                        sent_h: 0,
                    });
                }
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
        // P7-2 fail-closed: a CU request without credentials never reaches
        // the wire — honest error, never a silent skip.
        let wants_cu = req.tools.iter().any(|t| t.name == "computer");
        if wants_cu && self.api_key.trim().is_empty() {
            return Err(ProviderError::Transport(
                "anthropic computer-use: no API key — refusing to send CU request".into(),
            ));
        }

        let started = Instant::now();
        let mut call = self
            .agent
            .post(&self.base_url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", API_VERSION)
            .header("content-type", "application/json");
        // Hosted CU toolset declaration: beta header only on CU requests.
        if wants_cu {
            call = call.header("anthropic-beta", COMPUTER_TOOLSET_BETA);
        }
        let mut resp = call
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
        // The breakpoint sits on the last CACHEABLE segment, not the
        // trailing dynamic one (C5).
        assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
        assert!(body["system"][1].get("cache_control").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn param_filter_strips_cross_family_and_keeps_core() {
        // Cross-family key injected post-build is stripped for Claude.
        let system = vec![];
        let tools = vec![];
        let msgs = vec![Message::user_text("hi")];
        let mut body = Anthropic::build_body(&sample_req(&system, &tools, &msgs));
        body["reasoning_effort"] = json!("high");
        body["frequency_penalty"] = json!(0.5);
        crate::profile::strip_optional_params(
            &mut body,
            "claude-sonnet-5",
            &["reasoning_effort", "frequency_penalty", "thinking"],
        );
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("frequency_penalty").is_none());
        // Core keys are never stripped even when named.
        crate::profile::strip_optional_params(
            &mut body,
            "claude-sonnet-5",
            &["model", "messages", "tools", "max_tokens"],
        );
        for core in ["model", "messages", "tools", "max_tokens"] {
            assert!(body.get(core).is_some(), "{core} stripped");
        }
    }

    #[test]
    fn build_body_byte_stable_when_all_supported() {
        // All-supported path: filter touches nothing, bytes identical.
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
        let msgs = vec![Message::user_text("first")];
        let b1 = Anthropic::build_body(&sample_req(&system, &tools, &msgs));
        let mut b2 = b1.clone();
        crate::profile::strip_optional_params(
            &mut b2,
            "claude-sonnet-5",
            &[
                "temperature",
                "top_p",
                "top_k",
                "stop_sequences",
                "thinking",
            ],
        );
        assert_eq!(
            serde_json::to_string(&b1).unwrap(),
            serde_json::to_string(&b2).unwrap()
        );
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

    #[test]
    fn computer_toolset_beta_const() {
        // P7-2: hosted CU toolset declaration id is frozen.
        assert_eq!(COMPUTER_TOOLSET_BETA, "computer_toolset_20260801");
    }

    #[test]
    fn batched_computer_tool_use_parses_and_classifies() {
        // P7-2: batched tool_use incl. computer actions; classifier holds.
        let body = json!({
            "content": [
                {"type": "tool_use", "id": "tu_1", "name": "computer", "input": {"action": "screenshot"}},
                {"type": "tool_use", "id": "tu_2", "name": "computer", "input": {"action": "click", "x": 5, "y": 6}},
                {"type": "tool_use", "id": "tu_3", "name": "computer", "input": {"action": "type", "text": "hi", "cred_field": true}}
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let r = Anthropic::parse_response(&body, 10, 1).unwrap();
        assert_eq!(r.blocks.len(), 3);
        for b in &r.blocks {
            assert!(matches!(b, Block::ToolCall { name, .. } if name == "computer"));
        }
        let classes: Vec<_> = r
            .blocks
            .iter()
            .map(|b| match b {
                Block::ToolCall { input, .. } => crate::perm::classify("computer", input),
                _ => unreachable!(),
            })
            .collect();
        assert_eq!(
            classes,
            vec![
                crate::perm::Irreversibility::Read,
                crate::perm::Irreversibility::InternalWrite,
                crate::perm::Irreversibility::Identity,
            ]
        );
    }

    #[test]
    fn image_result_parses_to_image_block() {
        // P7-2: image-vs-text result — image content becomes Image.
        let body = json!({
            "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
            ],
            "stop_reason": "end_turn",
            "usage": {"input_tokens": 1, "output_tokens": 1}
        });
        let r = Anthropic::parse_response(&body, 10, 1).unwrap();
        assert!(matches!(r.blocks[0], Block::Image { .. }));
    }

    #[test]
    fn build_body_section_order_frozen() {
        // P7-2 R1-F3: system/tools/messages order untouched by CU.
        let system = vec![SystemSegment {
            name: "test",
            text: "s".into(),
            cacheable: true,
        }];
        let tools = vec![ToolSpec {
            name: "computer".into(),
            description: "cu".into(),
            input_schema: json!({"type": "object"}),
        }];
        let msgs = vec![Message::user_text("hi")];
        let body = Anthropic::build_body(&sample_req(&system, &tools, &msgs));
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert!(keys.contains(&"model"));
        assert!(keys.contains(&"max_tokens"));
        assert!(keys.contains(&"system"));
        assert!(keys.contains(&"tools"));
        assert!(keys.contains(&"messages"));
        // Image blocks serialize in message content, not as a new section.
        let img = Message {
            role: Role::User,
            content: vec![Block::Image {
                media_type: "image/png".into(),
                data_b64: "eA==".into(),
                px_w: 10,
                px_h: 10,
                sent_w: 5,
                sent_h: 5,
            }],
        };
        let wire = ir_message_to_wire(&img);
        assert_eq!(wire["content"][0]["type"], "image");
    }

    #[test]
    fn unconfigured_cu_request_errors_honestly() {
        // P7-2 fail-closed: empty key + computer tool → Transport, no wire.
        let a = Anthropic {
            agent: ureq::Agent::new_with_defaults(),
            api_key: String::new(),
            base_url: "http://127.0.0.1:9/none".into(),
        };
        let tools = vec![ToolSpec {
            name: "computer".into(),
            description: "cu".into(),
            input_schema: json!({"type": "object"}),
        }];
        let msgs = vec![Message::user_text("hi")];
        let system = vec![];
        let err = a.complete(&sample_req(&system, &tools, &msgs)).unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)));
    }

    /// C5 (invariant 2): the system breakpoint goes on the LAST cacheable
    /// segment — never on a trailing volatile one.
    #[test]
    fn system_breakpoint_on_last_cacheable_segment() {
        let seg = |text: &str, cacheable| SystemSegment {
            name: "test",
            text: text.into(),
            cacheable,
        };
        let system = vec![seg("a", true), seg("b", true), seg("volatile", false)];
        let msgs = vec![Message::user_text("hi")];
        let body = Anthropic::build_body(&sample_req(&system, &[], &msgs));
        assert_eq!(body["system"][1]["cache_control"]["type"], "ephemeral");
        assert!(body["system"][0].get("cache_control").is_none());
        assert!(body["system"][2].get("cache_control").is_none());
        // No cacheable segment → no system breakpoint at all.
        let dynamic_only = vec![seg("volatile", false)];
        let body = Anthropic::build_body(&sample_req(&dynamic_only, &[], &msgs));
        assert!(body["system"][0].get("cache_control").is_none());
    }
}
