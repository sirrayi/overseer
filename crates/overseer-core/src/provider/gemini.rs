//! Gemini generateContent adapter (playbook Ch.7 §1.3, P3.1).
//!
//! Wire conventions handled here:
//! - contents use roles `user`/`model`; tool calls are model-role
//!   `functionCall` parts, results are user-role `functionResponse` parts
//! - Gemini has NO call ids — pairing is by function name. `ToolCall.id`
//!   carries the name; the adapter strips nothing. Parallel same-name
//!   calls collapse into one wire-level pairing (a documented limit —
//!   the engine's own pairing stays intact either way).
//! - reasoning round-trips as opaque parts: `{"thought": true, ...,
//!   "thoughtSignature": "..."}` blobs are preserved verbatim in the IR
//!   `Reasoning` block and echoed back on the model turn — stripping
//!   them forfeits thinking continuity on tool-use turns (the "hard
//!   part" of the three-API IR).
//! - thinking effort maps to `generationConfig.thinkingConfig.
//!   thinkingBudget`; `includeThoughts` is on so signatures return.
//! - usageMetadata: promptTokenCount (includes cached), cachedContent-
//!   TokenCount, candidatesTokenCount, thoughtsTokenCount.

use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{Effort, Provider, ProviderError, Request, Response, StopReason};
use crate::ir::{Block, Message, Role, Usage};

const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta";

pub struct Gemini {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
}

impl Gemini {
    /// `base_url` is the API root (…/v1beta); `/models/{m}:generateContent`
    /// is appended per request.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(600)))
            .build();
        Gemini {
            agent: ureq::Agent::new_with_config(config),
            api_key: api_key.into(),
            base_url: base_url.into(),
        }
    }

    pub fn google(api_key: impl Into<String>) -> Self {
        Self::new(api_key, DEFAULT_BASE)
    }

    fn url(&self, model: &str) -> String {
        format!(
            "{}/models/{}:generateContent",
            self.base_url.trim_end_matches('/'),
            model
        )
    }

    fn build_body(req: &Request) -> Value {
        // Gemini takes a single system_instruction object — segments join
        // with a blank line (no per-segment breakpoints on this API; the
        // cachedContent path is implicit/context-cache only).
        let system = req
            .system
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        let mut contents = Vec::with_capacity(req.messages.len());
        for m in req.messages {
            ir_message_to_wire(m, &mut contents);
        }

        let tools: Vec<Value> = if req.tools.is_empty() {
            Vec::new()
        } else {
            vec![json!({
                "function_declarations": req.tools.iter().map(|t| {
                    // Gemini takes an OpenAPI-3 subset — full JSON-Schema
                    // keys (additionalProperties, default, …) 400 the
                    // whole request. Strip recursively.
                    let mut schema = t.input_schema.clone();
                    sanitize_schema(&mut schema);
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "parameters": schema,
                    })
                }).collect::<Vec<_>>()
            })]
        };

        let mut gen = json!({"maxOutputTokens": req.max_tokens});
        // Effort → thinkingBudget. Gemini's native range is 0–24576 on
        // 2.5 Pro; map the enum onto a coarse ladder. An explicit raw
        // thinking_budget wins over the enum.
        if let Some(budget) = req
            .thinking_budget
            .or_else(|| req.effort.map(effort_to_budget))
        {
            gen["thinkingConfig"] = json!({
                "thinkingBudget": budget,
                "includeThoughts": true,
            });
        }

        json!({
            "contents": contents,
            "tools": tools,
            "generationConfig": gen,
            "system_instruction": {"parts": [{"text": system}]},
        })
    }

    fn parse_response(
        body: &Value,
        request_bytes: u64,
        latency_ms: u64,
    ) -> Result<Response, ProviderError> {
        let cand = body
            .pointer("/candidates/0")
            .ok_or_else(|| ProviderError::Malformed("missing candidates[0]".into()))?;
        let parts = cand
            .pointer("/content/parts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut blocks = Vec::new();
        for p in parts {
            if p.get("thought").and_then(Value::as_bool) == Some(true) {
                // Whole part preserved: text AND thoughtSignature must
                // round-trip untouched.
                blocks.push(Block::Reasoning { raw: p.clone() });
                continue;
            }
            if let Some(fc) = p.get("functionCall") {
                let name = fc
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let mut input = fc.get("args").cloned().unwrap_or(json!({}));
                // P7-2 CU: per-step `safety_decision` rides alongside the
                // functionCall — the gate maps require_approval→Ask,
                // deny→Deny (fail-closed); absence means no safety hold.
                if let Some(sd) = p.get("safety_decision").or_else(|| fc.get("safety_decision")) {
                    input["safety_decision"] = sd.clone();
                }
                blocks.push(Block::ToolCall {
                    id: name.clone(), // pairing is by name on Gemini
                    name,
                    input,
                });
                continue;
            }
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                if !t.is_empty() {
                    blocks.push(Block::Text {
                        text: t.to_string(),
                    });
                }
                continue;
            }
            // Unknown part shape — preserve, don't drop.
            blocks.push(Block::Reasoning {
                raw: json!({"type": "unknown_part", "raw": p}),
            });
        }

        // Gemini has no dedicated tool-call finish reason — a functionCall
        // in the turn means ToolUse regardless of finishReason.
        let stop_reason = if blocks.iter().any(|b| matches!(b, Block::ToolCall { .. })) {
            StopReason::ToolUse
        } else {
            match cand.get("finishReason").and_then(Value::as_str) {
                Some("STOP") | Some("STOP_SEQUENCE") => StopReason::EndTurn,
                Some("MAX_TOKENS") => StopReason::MaxTokens,
                Some("SAFETY")
                | Some("PROHIBITED_CONTENT")
                | Some("RECITATION")
                | Some("BLOCKLIST")
                | Some("SPII") => StopReason::Refusal,
                Some(s) => StopReason::Other(s.to_string()),
                None => StopReason::Other("missing".to_string()),
            }
        };

        let u = body.get("usageMetadata").cloned().unwrap_or(json!({}));
        let prompt = u
            .get("promptTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let cached = u
            .get("cachedContentTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let candidates = u
            .get("candidatesTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let thoughts = u
            .get("thoughtsTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let usage = Usage {
            fresh_input: prompt.saturating_sub(cached),
            cache_write: 0, // implicit caching reports no write count
            cache_read: cached,
            output: candidates,
            reasoning: thoughts,
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

/// Strip JSON-Schema keys the Gemini API rejects (verified live: a
/// single `additionalProperties` anywhere in the tree → 400 on the whole
/// request). Objects recurse, arrays recurse, scalars pass.
fn sanitize_schema(v: &mut Value) {
    const UNSUPPORTED: &[&str] = &[
        "additionalProperties",
        "default",
        "propertyOrdering",
        "minItems",
        "maxItems",
        "pattern",
        "minLength",
        "maxLength",
    ];
    match v {
        Value::Object(m) => {
            for k in UNSUPPORTED {
                m.remove(*k);
            }
            for (_, child) in m.iter_mut() {
                sanitize_schema(child);
            }
        }
        Value::Array(a) => {
            for child in a.iter_mut() {
                sanitize_schema(child);
            }
        }
        _ => {}
    }
}

/// Effort → Gemini thinkingBudget (2.5-series native range 0–24576).
pub fn effort_to_budget(e: Effort) -> u32 {
    match e {
        Effort::Min => 0,
        Effort::Low => 1_024,
        Effort::Medium => 4_096,
        Effort::High => 12_288,
        Effort::Max => 24_576,
    }
}

/// P7-2 CU safety gate: map a per-step `safety_decision` to a gate verdict
/// (incl. absent) → None (existing rules decide). Pure + tested.
pub fn safety_gate(input: &Value) -> Option<crate::perm::Verdict> {
    let d = input.get("safety_decision").and_then(Value::as_str)?;
    match d.to_ascii_lowercase().as_str() {
        "require_approval" | "ask" | "approval_required" => Some(crate::perm::Verdict::Ask {
            reason: "gemini computer-use: safety check requires approval".into(),
        }),
        "deny" | "block" | "blocked" => Some(crate::perm::Verdict::Deny {
            reason: "gemini computer-use: safety check denied the action".into(),
        }),
        _ => None,
    }
}

/// One IR message → one wire content (role user|model). Tool results
/// join the SAME user content as functionResponse parts — Gemini wants
/// them grouped per turn, not fanned out like OpenAI's `tool` role.
fn ir_message_to_wire(m: &Message, out: &mut Vec<Value>) {
    let role = match m.role {
        Role::User => "user",
        Role::Assistant => "model",
    };
    let mut parts: Vec<Value> = Vec::new();
    for b in &m.content {
        match b {
            Block::Text { text } => parts.push(json!({"text": text})),
            // Opaque reasoning/thought parts echo back verbatim —
            // thoughtSignature continuity is load-bearing on tool turns.
            Block::Reasoning { raw } => parts.push(raw.clone()),
            // P7-1: screenshots ride as inline_data parts (native CU shape).
            Block::Image { media_type, data_b64, .. } => parts.push(json!({
                "inline_data": {"mime_type": media_type, "data": data_b64}
            })),
            Block::ToolCall { name, input, .. } => parts.push(json!({
                "functionCall": {"name": name, "args": input}
            })),
            Block::ToolResult {
                tool_use_id,
                content,
                ..
            } => parts.push(json!({
                "functionResponse": {
                    "name": tool_use_id, // the id IS the name (see module docs)
                    "response": {"result": content},
                }
            })),
        }
    }
    if !parts.is_empty() {
        out.push(json!({"role": role, "parts": parts}));
    }
}

impl Provider for Gemini {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = Self::build_body(req);
        let request_bytes = body.to_string().len() as u64;
        // P7-2 fail-closed: a CU request without credentials never reaches
        // the wire — honest error, never a silent skip.
        if req.tools.iter().any(|t| t.name == "computer") && self.api_key.trim().is_empty() {
            return Err(ProviderError::Transport(
                "gemini computer-use: no API key — refusing to send CU request".into(),
            ));
        }
        let started = Instant::now();
        let mut resp = self
            .agent
            .post(&self.url(req.model))
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json")
            .send_json(&body)
            .map_err(|e| ProviderError::Transport(e.to_string()))?;
        let latency_ms = started.elapsed().as_millis() as u64;

        let status = resp.status().as_u16();
        let text = resp
            .body_mut()
            .read_to_string()
            .map_err(|e| ProviderError::Transport(e.to_string()))?;

        if status == 429 || status == 503 {
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
        "gemini"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{SystemSegment, ToolSpec};

    fn sample_req<'a>(
        system: &'a [SystemSegment],
        tools: &'a [ToolSpec],
        msgs: &'a [Message],
    ) -> Request<'a> {
        Request {
            model: "gemini-3-pro",
            system,
            tools,
            messages: msgs,
            max_tokens: 8192,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: false,
        }
    }

    #[test]
    fn body_shape_and_tool_decls() {
        let system = vec![SystemSegment {
            name: "test",
            text: "sys".into(),
            cacheable: true,
        }];
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "d".into(),
            input_schema: json!({"type": "object"}),
        }];
        let msgs = vec![Message::user_text("hi")];
        let body = Gemini::build_body(&sample_req(&system, &tools, &msgs));
        assert_eq!(body["system_instruction"]["parts"][0]["text"], "sys");
        assert_eq!(body["contents"][0]["role"], "user");
        assert_eq!(body["tools"][0]["function_declarations"][0]["name"], "read");
        assert_eq!(body["generationConfig"]["maxOutputTokens"], 8192);
    }

    #[test]
    fn thought_signature_roundtrips_verbatim() {
        let thought = json!({
            "thought": true,
            "text": "reasoning",
            "thoughtSignature": "sig-xyz"
        });
        let m = Message {
            role: Role::Assistant,
            content: vec![Block::Reasoning {
                raw: thought.clone(),
            }],
        };
        let mut out = Vec::new();
        ir_message_to_wire(&m, &mut out);
        assert_eq!(out[0]["role"], "model");
        assert_eq!(out[0]["parts"][0], thought);
    }

    #[test]
    fn tool_result_pairs_by_name_in_user_turn() {
        let m = Message::tool_results(vec![
            Block::ToolResult {
                tool_use_id: "read".into(),
                content: "contents".into(),
                is_error: false,
            },
            Block::ToolResult {
                tool_use_id: "grep".into(),
                content: "hits".into(),
                is_error: true,
            },
        ]);
        let mut out = Vec::new();
        ir_message_to_wire(&m, &mut out);
        assert_eq!(out.len(), 1, "results group into one user turn");
        assert_eq!(out[0]["parts"][0]["functionResponse"]["name"], "read");
        assert_eq!(
            out[0]["parts"][1]["functionResponse"]["response"]["result"],
            "hits"
        );
    }

    #[test]
    fn parse_thought_plus_function_call() {
        let body = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"thought": true, "thoughtSignature": "s1"},
                    {"functionCall": {"name": "read", "args": {"path": "a.rs"}}},
                    {"text": "reading it"}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "cachedContentTokenCount": 60,
                "candidatesTokenCount": 30,
                "thoughtsTokenCount": 12
            }
        });
        let r = Gemini::parse_response(&body, 10, 5).unwrap();
        // A functionCall in the turn ⇒ ToolUse even though STOP.
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert_eq!(r.usage.fresh_input, 40);
        assert_eq!(r.usage.cache_read, 60);
        assert_eq!(r.usage.reasoning, 12);
        assert!(matches!(r.blocks[0], Block::Reasoning { .. }));
        assert!(matches!(r.blocks[1], Block::ToolCall { ref id, .. } if id == "read"));
    }

    #[test]
    fn effort_maps_to_thinking_budget() {
        let system = vec![SystemSegment {
            name: "test",
            text: "s".into(),
            cacheable: true,
        }];
        let msgs = vec![Message::user_text("x")];
        let mut req = sample_req(&system, &[], &msgs);
        req.effort = Some(Effort::High);
        let body = Gemini::build_body(&req);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            12_288
        );
        // Raw budget wins over the enum.
        req.thinking_budget = Some(777);
        let body = Gemini::build_body(&req);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            777
        );
    }

    #[test]
    fn schema_sanitizer_strips_recursively() {
        let mut s = json!({
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {"x": {"type": "string", "default": "d"}}
                    }
                }
            }
        });
        sanitize_schema(&mut s);
        assert!(s.get("additionalProperties").is_none());
        let nested = &s["properties"]["items"]["items"];
        assert!(nested.get("additionalProperties").is_none());
        assert!(nested["properties"]["x"].get("default").is_none());
        // Supported keys survive.
        assert_eq!(s["properties"]["items"]["type"], "array");
    }

    #[test]
    fn safety_finish_maps_to_refusal() {
        let body = json!({
            "candidates": [{"content": {"parts": [{"text": ""}]},
                "finishReason": "SAFETY"}]
        });
        let r = Gemini::parse_response(&body, 0, 0).unwrap();
        assert_eq!(r.stop_reason, StopReason::Refusal);
    }

    #[test]
    fn function_call_with_safety_decision_routes_to_gate() {
        // P7-2: per-step function_call + safety_decision → Ask/Deny.
        let body = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"functionCall": {"name": "computer", "args": {"action": "click"}},
                     "safety_decision": "require_approval"}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 5, "candidatesTokenCount": 5}
        });
        let r = Gemini::parse_response(&body, 0, 0).unwrap();
        let input = match &r.blocks[0] {
            Block::ToolCall { input, .. } => input.clone(),
            _ => panic!("expected ToolCall"),
        };
        assert_eq!(input["safety_decision"], "require_approval");
        assert!(matches!(
            safety_gate(&input),
            Some(crate::perm::Verdict::Ask { .. })
        ));
        assert!(matches!(
            safety_gate(&json!({"safety_decision": "deny"})),
            Some(crate::perm::Verdict::Deny { .. })
        ));
        assert!(safety_gate(&json!({"action": "click"})).is_none());
    }

    #[test]
    fn unconfigured_cu_request_errors_honestly() {
        // P7-2 fail-closed: empty key + computer tool → Transport, no wire.
        let g = Gemini::new("", "http://127.0.0.1:9/v1beta");
        let tools = vec![ToolSpec {
            name: "computer".into(),
            description: "cu".into(),
            input_schema: json!({"type": "object"}),
        }];
        let msgs = vec![Message::user_text("hi")];
        let system = vec![];
        let err = g.complete(&sample_req(&system, &tools, &msgs)).unwrap_err();
        assert!(matches!(err, ProviderError::Transport(_)));
    }
}
