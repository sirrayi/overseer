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

use std::time::Instant;

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
        Gemini {
            agent: super::http_agent(),
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
        // Param filter (Ch.7 §2.2): drop generationConfig keys the profile
        // rejects (unknown models keep the full Gemini set via FALLBACK).
        // maxOutputTokens is a core key — never stripped (supports_param
        // refuses it) — but listed for symmetry with the other adapters.
        crate::profile::strip_optional_params(
            &mut gen,
            req.model,
            &[
                "temperature",
                "topP",
                "topK",
                "maxOutputTokens",
                "thinkingConfig",
            ],
        );

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
                let mut input = super::object_input(fc.get("args").cloned().unwrap_or(json!({})));
                // P7-2 CU: per-step `safety_decision` rides alongside the
                // functionCall — the gate maps require_approval→Ask,
                // deny→Deny (fail-closed); absence means no safety hold.
                if let Some(sd) = p
                    .get("safety_decision")
                    .or_else(|| fc.get("safety_decision"))
                {
                    input["safety_decision"] = sd.clone();
                }
                // The signed part rides opaque ahead of its call; the echo
                // re-attaches its thoughtSignature (invariant 6).
                if p.get("thoughtSignature").is_some() {
                    blocks.push(Block::Reasoning { raw: p.clone() });
                }
                blocks.push(Block::ToolCall {
                    id: name.clone(), // pairing is by name on Gemini
                    name,
                    input,
                });
                continue;
            }
            if let Some(t) = p.get("text").and_then(Value::as_str) {
                // A signed text part rides opaque (the echo sends it back
                // verbatim); the Text block beside it is the view's copy.
                if p.get("thoughtSignature").is_some() {
                    blocks.push(Block::Reasoning { raw: p.clone() });
                }
                if !t.is_empty() {
                    blocks.push(Block::Text {
                        text: t.to_string(),
                    });
                }
                continue;
            }
            // Unknown part shape — preserve its exact wire form.
            blocks.push(Block::Reasoning { raw: p });
        }

        // Gemini has no dedicated tool-call finish reason — a functionCall
        // in the turn means ToolUse regardless of finishReason.
        let stop_reason = if blocks.iter().any(|b| matches!(b, Block::ToolCall { .. })) {
            StopReason::ToolUse
        } else {
            match cand.get("finishReason").and_then(Value::as_str) {
                Some("STOP") | Some("STOP_SEQUENCE") => StopReason::EndTurn,
                Some("MAX_TOKENS") => StopReason::MaxTokens,
                Some(
                    s @ ("SAFETY" | "PROHIBITED_CONTENT" | "RECITATION" | "BLOCKLIST" | "SPII"),
                ) => StopReason::Blocked(s.to_string()),
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

/// The text of a signed, non-thought text part (empty text included).
fn signed_text(raw: &Value) -> Option<&str> {
    if raw.get("thought").and_then(Value::as_bool) == Some(true)
        || raw.get("thoughtSignature").is_none()
    {
        return None;
    }
    raw.get("text").and_then(Value::as_str)
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
    let mut fc_signature: Option<Value> = None;
    let mut echoed_text: Option<&str> = None;
    for b in &m.content {
        // A signed text part was already sent verbatim; skip the Text
        // block that duplicates it for the view.
        let duplicate = echoed_text.take();
        match b {
            Block::Text { text } if duplicate == Some(text.as_str()) => {}
            Block::Text { text } => parts.push(json!({"text": text})),
            Block::Reasoning { raw } if raw.get("functionCall").is_some() => {
                fc_signature = raw.get("thoughtSignature").cloned();
            }
            // Logs written before unknown parts were stored raw.
            Block::Reasoning { raw }
                if raw.get("type").and_then(Value::as_str) == Some("unknown_part")
                    && raw.get("raw").is_some() =>
            {
                parts.push(raw["raw"].clone());
            }
            // Opaque reasoning/thought parts echo back verbatim —
            // thoughtSignature continuity is load-bearing on tool turns.
            Block::Reasoning { raw } => {
                echoed_text = signed_text(raw);
                parts.push(raw.clone());
            }
            // P7-1: screenshots ride as inline_data parts (native CU shape).
            Block::Image {
                media_type,
                data_b64,
                ..
            } => parts.push(json!({
                "inline_data": {"mime_type": media_type, "data": data_b64}
            })),
            Block::ToolCall { name, input, .. } => {
                let mut part = json!({"functionCall": {"name": name, "args": input}});
                if let Some(sig) = fc_signature.take() {
                    part["thoughtSignature"] = sig;
                }
                parts.push(part);
            }
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
        let call = self
            .agent
            .post(&self.url(req.model))
            .header("x-goog-api-key", &self.api_key)
            .header("content-type", "application/json");
        let (parsed, latency_ms) = super::send_json(call, &body, started, &[429, 503])?;
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
            cache_key: None,
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
    fn param_filter_strips_cross_family_gen_keys() {
        // "gemini-3-pro" has no tabled row → FALLBACK union (accepts Gemini
        // keys). Prove the filter path directly: a strict profile
        // (deepseek-v4.1-flash rejects everything Gemini-spelled) strips.
        let mut gen = json!({
            "maxOutputTokens": 100,
            "thinkingConfig": {"thinkingBudget": 777},
            "reasoning_effort": "high",
        });
        crate::profile::strip_optional_params(
            &mut gen,
            "deepseek-v4.1-flash",
            &["reasoning_effort", "frequency_penalty", "thinkingConfig"],
        );
        assert!(gen.get("reasoning_effort").is_none());
        assert!(gen.get("thinkingConfig").is_none());
        // Core key survives even when named.
        crate::profile::strip_optional_params(
            &mut gen,
            "deepseek-v4.1-flash",
            &["maxOutputTokens"],
        );
        assert!(gen.get("maxOutputTokens").is_some());
        // And the real Gemini path keeps thinkingConfig via FALLBACK.
        let system = vec![SystemSegment {
            name: "test",
            text: "s".into(),
            cacheable: true,
        }];
        let msgs = vec![Message::user_text("x")];
        let tools: Vec<ToolSpec> = vec![];
        let req = Request {
            model: "gemini-3-pro",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 100,
            thinking_budget: Some(777),
            effort: None,
            cache_breakpoints: false,
            cache_key: None,
        };
        let body = Gemini::build_body(&req);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            777
        );
        // Schema sanitizer still strips 400-trigger keys (no regress).
        let mut s = json!({"type": "object", "additionalProperties": false});
        sanitize_schema(&mut s);
        assert!(s.get("additionalProperties").is_none());
    }

    #[test]
    fn build_body_byte_stable_when_all_supported() {
        // Unknown model → FALLBACK union: thinkingConfig survives, and a
        // re-strip of the supported set is a byte-identical no-op.
        let system = vec![SystemSegment {
            name: "test",
            text: "s".into(),
            cacheable: true,
        }];
        let msgs = vec![Message::user_text("x")];
        let tools: Vec<ToolSpec> = vec![];
        let req = Request {
            model: "some-future-model",
            system: &system,
            tools: &tools,
            messages: &msgs,
            max_tokens: 100,
            thinking_budget: Some(777),
            effort: None,
            cache_breakpoints: false,
            cache_key: None,
        };
        let b1 = Gemini::build_body(&req);
        assert_eq!(
            b1["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            777
        );
        let mut b2 = b1.clone();
        crate::profile::strip_optional_params(
            &mut b2["generationConfig"],
            "some-future-model",
            &[
                "temperature",
                "topP",
                "topK",
                "maxOutputTokens",
                "thinkingConfig",
            ],
        );
        assert_eq!(
            serde_json::to_string(&b1).unwrap(),
            serde_json::to_string(&b2).unwrap()
        );
    }

    #[test]
    fn safety_finish_maps_to_refusal() {
        let body = json!({
            "candidates": [{"content": {"parts": [{"text": ""}]},
                "finishReason": "SAFETY"}]
        });
        let r = Gemini::parse_response(&body, 0, 0).unwrap();
        assert!(r.stop_reason.is_refusal());
        assert_eq!(r.stop_reason.as_str(), "SAFETY");
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

    /// C3: `args` from the wire may be a string/array — attaching the
    /// safety decision must not panic, and the raw value survives.
    #[test]
    fn non_object_args_with_safety_decision_preserved() {
        for args in [json!("raw string"), json!([1, 2])] {
            let body = json!({
                "candidates": [{
                    "content": {"role": "model", "parts": [
                        {"functionCall": {"name": "computer", "args": args.clone()},
                         "safety_decision": "require_approval"}
                    ]},
                    "finishReason": "STOP"
                }]
            });
            let r = Gemini::parse_response(&body, 0, 0).unwrap();
            match &r.blocks[0] {
                Block::ToolCall { input, .. } => {
                    assert_eq!(input["_unparsed"], args);
                    assert_eq!(input["safety_decision"], "require_approval");
                }
                other => panic!("expected ToolCall, got {other:?}"),
            }
        }
    }

    fn model_reply(parts: Value) -> Value {
        json!({"candidates": [{"content": {"role": "model", "parts": parts},
            "finishReason": "STOP"}]})
    }

    fn echo(blocks: Vec<Block>) -> Value {
        let m = Message {
            role: Role::Assistant,
            content: blocks,
        };
        let mut out = Vec::new();
        ir_message_to_wire(&m, &mut out);
        out[0]["parts"].clone()
    }

    #[test]
    fn signed_text_part_roundtrips_verbatim() {
        let parts = json!([{"text": "the answer", "thoughtSignature": "SIG-TEXT"}]);
        let r = Gemini::parse_response(&model_reply(parts.clone()), 0, 0).unwrap();
        assert!(matches!(&r.blocks[0], Block::Reasoning { raw } if raw == &parts[0]));
        assert!(matches!(&r.blocks[1], Block::Text { text } if text == "the answer"));
        assert_eq!(r.blocks.len(), 2);
        assert_eq!(echo(r.blocks), parts);
        // A signature-only empty text part is kept, and echoes alone.
        let empty = json!([{"text": "", "thoughtSignature": "SIG-EMPTY"}, {"text": "after"}]);
        let r = Gemini::parse_response(&model_reply(empty.clone()), 0, 0).unwrap();
        assert!(matches!(&r.blocks[0], Block::Reasoning { raw } if raw == &empty[0]));
        assert_eq!(r.blocks.len(), 2);
        assert_eq!(echo(r.blocks), empty);
        // Unsigned text is unchanged: one Text block, one plain part.
        let plain = json!([{"text": "hi"}]);
        let r = Gemini::parse_response(&model_reply(plain.clone()), 0, 0).unwrap();
        assert_eq!(r.blocks.len(), 1);
        assert_eq!(echo(r.blocks), plain);
    }

    #[test]
    fn unknown_part_is_stored_raw_and_echoed_verbatim() {
        let part = json!({"executableCode": {"language": "PYTHON", "code": "1+1"}});
        let r = Gemini::parse_response(&model_reply(json!([part.clone()])), 0, 0).unwrap();
        assert!(matches!(&r.blocks[0], Block::Reasoning { raw } if raw == &part));
        assert_eq!(echo(r.blocks), json!([part.clone()]));
        // Old logs carry the wrapped form; the echo unwraps it.
        let old = vec![Block::Reasoning {
            raw: json!({"type": "unknown_part", "raw": part.clone()}),
        }];
        assert_eq!(echo(old), json!([part]));
    }

    #[test]
    fn unchanged_history_builds_byte_identical_bodies() {
        let parts = json!([
            {"thought": true, "text": "plan", "thoughtSignature": "SIG-T"},
            {"text": "the answer", "thoughtSignature": "SIG-TEXT"},
            {"executableCode": {"language": "PYTHON", "code": "1+1"}},
            {"functionCall": {"name": "read", "args": {"p": "a"}}, "thoughtSignature": "SIG-FC"}
        ]);
        let r = Gemini::parse_response(&model_reply(parts.clone()), 0, 0).unwrap();
        let msgs = vec![
            Message::user_text("go"),
            Message {
                role: Role::Assistant,
                content: r.blocks,
            },
            Message::tool_results(vec![Block::ToolResult {
                tool_use_id: "read".into(),
                content: "A".into(),
                is_error: false,
            }]),
        ];
        let system = vec![SystemSegment {
            name: "test",
            text: "s".into(),
            cacheable: true,
        }];
        let first = Gemini::build_body(&sample_req(&system, &[], &msgs)).to_string();
        let second = Gemini::build_body(&sample_req(&system, &[], &msgs)).to_string();
        assert_eq!(first, second);
        let body: Value = serde_json::from_str(&first).unwrap();
        assert_eq!(
            body["contents"][1]["parts"], parts,
            "model turn echoes the wire parts"
        );
    }
}
