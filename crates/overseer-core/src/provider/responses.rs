//! OpenAI Responses API adapter (`POST {base}/responses`).
//!
//! Distinct wire shape from chat/completions: `instructions` + flat `input`
//! item list in, `output[]` items (reasoning / message / function_call) out.
//! Needed for models that are Responses-only — e.g. opencode Go's
//! `muse-spark-*` (chat/completions 500s upstream; verified 2026-09).
//!
//! Encrypted reasoning round-trips through the opaque `Block::Reasoning`
//! invariant: the whole wire item (id + encrypted_content + summary) is
//! stored as `raw` and re-emitted verbatim on the next call — the harness
//! never inspects it. No streaming, no `previous_response_id` chaining:
//! the event log is the source of truth, so every call carries full
//! history (`store: false` keeps server-side state off).

use std::collections::HashSet;
use std::time::Instant;

use serde_json::{json, Value};

use super::{Effort, Provider, ProviderError, Request, Response, StopReason};
use crate::ir::{Block, Message, Role, Usage};

const DEFAULT_URL: &str = "https://api.openai.com/v1";

pub struct ResponsesApi {
    agent: ureq::Agent,
    api_key: String,
    base_url: String,
    /// Same mechanism as `OpenAiCompatible::extra_headers` — opencode Go
    /// requires `x-opencode-session` on this endpoint too.
    extra_headers: Vec<(String, String)>,
}

impl ResponsesApi {
    /// `base_url` is the versioned root (…/v1); the path is appended.
    pub fn new(api_key: impl Into<String>, base_url: impl Into<String>) -> Self {
        ResponsesApi {
            agent: super::http_agent(),
            api_key: api_key.into(),
            base_url: base_url.into(),
            extra_headers: Vec::new(),
        }
    }

    pub fn openai(api_key: impl Into<String>) -> Self {
        Self::new(api_key, DEFAULT_URL)
    }

    /// Attach an extra header sent on every request.
    pub fn with_header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.extra_headers.push((name.into(), value.into()));
        self
    }

    fn url(&self) -> String {
        format!("{}/responses", self.base_url.trim_end_matches('/'))
    }

    fn effort_str(e: Effort) -> &'static str {
        // Responses takes named effort, not a token budget. Max clamps to
        // "high" — the highest value the public API documents.
        match e {
            Effort::Min | Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High | Effort::Max => "high",
        }
    }

    fn build_body(req: &Request) -> Value {
        let instructions = req
            .system
            .iter()
            .map(|s| s.text.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");

        // Computer results are typed by their originating call's name
        // (call id → name), never by sniffing the result text.
        let computer_calls: HashSet<&str> = req
            .messages
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                Block::ToolCall { id, name, .. } if name == "computer" => Some(id.as_str()),
                _ => None,
            })
            .collect();
        let mut input = Vec::with_capacity(req.messages.len() + 2);
        for m in req.messages {
            ir_message_to_input(m, &computer_calls, &mut input);
        }

        let tools: Vec<Value> = req
            .tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                    "strict": false,
                })
            })
            .collect();

        let mut body = json!({
            "model": req.model,
            "instructions": instructions,
            "input": input,
            "max_output_tokens": req.max_tokens,
            // No server-side state — the event log replays full history.
            "store": false,
            // Ask for encrypted reasoning back so multi-turn round-trips.
            "include": ["reasoning.encrypted_content"],
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        // thinking_budget has no Responses equivalent: requesting a token
        // budget just selects high effort (it "wins over effort" per the
        // Request contract). Cache breakpoints are a no-op — Go caches
        // prefixes automatically if it does at all.
        if req.thinking_budget.is_some() {
            body["reasoning"] = json!({"effort": "high"});
        } else if let Some(e) = req.effort {
            body["reasoning"] = json!({"effort": Self::effort_str(e)});
        }
        if let Some(k) = &req.cache_key {
            if crate::profile::supports_param(req.model, "prompt_cache_key") {
                body["prompt_cache_key"] = json!(k);
            }
        }
        body
    }

    fn parse_response(
        parsed: &Value,
        request_bytes: u64,
        latency_ms: u64,
    ) -> Result<Response, ProviderError> {
        let mut blocks = Vec::new();
        let mut saw_tool_call = false;
        let response_id = parsed["id"].clone();
        for item in parsed["output"].as_array().cloned().unwrap_or_default() {
            match item["type"].as_str() {
                Some("reasoning") => blocks.push(Block::Reasoning { raw: item }),
                Some("message") => {
                    for part in item["content"].as_array().cloned().unwrap_or_default() {
                        if part["type"].as_str() == Some("output_text") {
                            blocks.push(Block::Text {
                                text: part["text"].as_str().unwrap_or("").to_string(),
                            });
                        }
                    }
                }
                // P7-2 CU: a `computer_call` item decodes to a `computer`
                // ToolCall; the linkage (`previous_response_id` = this
                // response's id, `pending_safety_checks`) rides in the input
                // so the ack gate can hold the turn.
                Some("computer_call") => {
                    saw_tool_call = true;
                    let mut input =
                        super::object_input(item.get("action").cloned().unwrap_or(json!({})));
                    if !response_id.is_null() {
                        input["previous_response_id"] = response_id.clone();
                    }
                    if let Some(checks) = item.get("pending_safety_checks") {
                        input["pending_safety_checks"] = checks.clone();
                    }
                    blocks.push(Block::ToolCall {
                        id: item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: "computer".into(),
                        input,
                    });
                }
                Some("function_call") => {
                    saw_tool_call = true;
                    // Unparseable (or non-object) arguments are preserved
                    // raw, mirroring the chat adapter.
                    let input = match item["arguments"].as_str() {
                        Some(a) => serde_json::from_str(a)
                            .map(super::object_input)
                            .unwrap_or_else(|_| json!({"_unparsed": a})),
                        None => json!({}),
                    };
                    blocks.push(Block::ToolCall {
                        // `call_id` is what function_call_output references;
                        // the item `id` (fc_…) is server bookkeeping.
                        id: item["call_id"]
                            .as_str()
                            .or_else(|| item["id"].as_str())
                            .unwrap_or("")
                            .to_string(),
                        name: item["name"].as_str().unwrap_or("").to_string(),
                        input,
                    });
                }
                _ => {}
            }
        }

        let usage = &parsed["usage"];
        let input_tokens = usage["input_tokens"].as_u64().unwrap_or(0);
        let cached = usage["input_tokens_details"]["cached_tokens"]
            .as_u64()
            .unwrap_or(0);
        let cache_write = usage["input_tokens_details"]["cache_write_tokens"]
            .as_u64()
            .unwrap_or(0);
        let output_tokens = usage["output_tokens"].as_u64().unwrap_or(0);
        let reasoning_tokens = usage["output_tokens_details"]["reasoning_tokens"]
            .as_u64()
            .unwrap_or(0);

        // status: "completed" | "incomplete" | "failed" | "cancelled" |
        // "in_progress". incomplete_details.reason tells us *why*
        // (max_output_tokens, content_filter, …). Anything unmapped
        // survives raw in `Other` (invariant 7).
        let stop_reason = if saw_tool_call {
            StopReason::ToolUse
        } else {
            match parsed["status"].as_str() {
                Some("completed") => StopReason::EndTurn,
                Some("incomplete") => match parsed["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => StopReason::MaxTokens,
                    Some("content_filter") => StopReason::Refusal,
                    Some(r) => StopReason::Other(r.to_string()),
                    None => StopReason::Other("incomplete".to_string()),
                },
                Some(s) => StopReason::Other(s.to_string()),
                None => StopReason::Other("missing".to_string()),
            }
        };

        Ok(Response {
            blocks,
            stop_reason,
            usage: Usage {
                fresh_input: input_tokens
                    .saturating_sub(cached)
                    .saturating_sub(cache_write),
                cache_read: cached,
                cache_write,
                output: output_tokens.saturating_sub(reasoning_tokens),
                reasoning: reasoning_tokens,
            },
            request_bytes,
            latency_ms,
        })
    }
}

/// IR → Responses input items. Tool results and tool calls are top-level
/// items (not nested in messages); reasoning items echo back verbatim.
/// `computer_calls` holds the ids of `computer` ToolCalls in the request,
/// so their results go back as `computer_call_output`.
fn ir_message_to_input(m: &Message, computer_calls: &HashSet<&str>, out: &mut Vec<Value>) {
    match m.role {
        Role::User => {
            let mut parts: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    Block::Text { text } => parts.push(json!({
                        "type": "input_text", "text": text,
                    })),
                    Block::Image {
                        media_type,
                        data_b64,
                        ..
                    } => parts.push(json!({
                        "type": "input_image",
                        "image_url": format!("data:{media_type};base64,{data_b64}"),
                    })),
                    Block::ToolResult {
                        tool_use_id,
                        content,
                        ..
                    } => out.push(json!({
                        "type": if computer_calls.contains(tool_use_id.as_str()) {
                            "computer_call_output"
                        } else {
                            "function_call_output"
                        },
                        "call_id": tool_use_id,
                        "output": content,
                    })),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                out.push(json!({
                    "type": "message", "role": "user", "content": parts,
                }));
            }
        }
        Role::Assistant => {
            let mut parts: Vec<Value> = Vec::new();
            for b in &m.content {
                match b {
                    // Opaque round-trip — the stored wire item re-emits
                    // untouched (invariant 6).
                    Block::Reasoning { raw } => out.push(raw.clone()),
                    Block::Text { text } => parts.push(json!({
                        "type": "output_text", "text": text,
                    })),
                    Block::ToolCall { id, name, input } if name == "computer" => {
                        out.push(computer_call_item(id, input))
                    }
                    Block::ToolCall { id, name, input } => out.push(json!({
                        "type": "function_call",
                        "call_id": id,
                        "name": name,
                        "arguments": input.to_string(),
                    })),
                    _ => {}
                }
            }
            if !parts.is_empty() {
                out.push(json!({
                    "type": "message", "role": "assistant", "content": parts,
                }));
            }
        }
    }
}

/// A `computer` ToolCall re-emitted as a Responses `computer_call` item:
/// the harness-side linkage keys are lifted back out of the input.
// DEFERRED(owner): full computer_call round-trip fidelity (server item `id`, `acknowledged_safety_checks`, the `computer_screenshot` output payload) — gate: a recorded live Responses CU fixture.
fn computer_call_item(id: &str, input: &Value) -> Value {
    let mut action = input.clone();
    let mut checks = json!([]);
    let mut raw = None;
    if let Some(obj) = action.as_object_mut() {
        obj.remove("previous_response_id");
        obj.remove("safety_ack");
        if let Some(c) = obj.remove("pending_safety_checks") {
            checks = c;
        }
        if obj.len() == 1 {
            raw = obj.get("_unparsed").cloned();
        }
    }
    json!({
        "type": "computer_call",
        "call_id": id,
        "action": raw.unwrap_or(action),
        "pending_safety_checks": checks,
    })
}

impl Provider for ResponsesApi {
    fn complete(&self, req: &Request) -> Result<Response, ProviderError> {
        let body = Self::build_body(req);
        let request_bytes = body.to_string().len() as u64;

        let started = Instant::now();
        let call = super::bearer_post(&self.agent, &self.url(), &self.api_key, &self.extra_headers);
        let (parsed, latency_ms) = super::send_json(call, &body, started, super::RATE_LIMITED)?;
        Self::parse_response(&parsed, request_bytes, latency_ms)
    }

    fn name(&self) -> &'static str {
        "openai-responses"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{SystemSegment, ToolSpec};

    fn bare_req<'a>(
        system: &'a [SystemSegment],
        tools: &'a [ToolSpec],
        msgs: &'a [Message],
    ) -> Request<'a> {
        Request {
            model: "muse-spark-1.3-contributor",
            system,
            tools,
            messages: msgs,
            max_tokens: 1024,
            thinking_budget: None,
            effort: None,
            cache_breakpoints: false,
            cache_key: None,
        }
    }

    /// K4: `prompt_cache_key` only where the profile accepts it — official
    /// OpenAI models, never the gateway-hosted muse rows.
    #[test]
    fn prompt_cache_key_only_on_openai_profiles() {
        let msgs = vec![Message::user_text("hi")];
        let mut req = bare_req(&[], &[], &msgs);
        req.cache_key = Some("sess-1".into());
        assert!(ResponsesApi::build_body(&req)
            .get("prompt_cache_key")
            .is_none());
        req.model = "gpt-5.5";
        assert_eq!(ResponsesApi::build_body(&req)["prompt_cache_key"], "sess-1");
    }

    #[test]
    fn tool_result_becomes_function_call_output() {
        let m = Message::tool_results(vec![Block::ToolResult {
            tool_use_id: "call_1".into(),
            content: "r1".into(),
            is_error: false,
        }]);
        let mut out = Vec::new();
        ir_message_to_input(&m, &Default::default(), &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["type"], "function_call_output");
        assert_eq!(out[0]["call_id"], "call_1");
        assert_eq!(out[0]["output"], "r1");
    }

    #[test]
    fn assistant_tool_call_and_text_split_items() {
        let m = Message {
            role: Role::Assistant,
            content: vec![
                Block::Text {
                    text: "let me check".into(),
                },
                Block::ToolCall {
                    id: "call_9".into(),
                    name: "bash".into(),
                    input: json!({"command": "ls"}),
                },
            ],
        };
        let mut out = Vec::new();
        ir_message_to_input(&m, &Default::default(), &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0]["type"], "function_call");
        assert_eq!(out[0]["call_id"], "call_9");
        let args: Value = serde_json::from_str(out[0]["arguments"].as_str().unwrap()).unwrap();
        assert_eq!(args["command"], "ls");
        assert_eq!(out[1]["type"], "message");
        assert_eq!(out[1]["role"], "assistant");
        assert_eq!(out[1]["content"][0]["type"], "output_text");
    }

    #[test]
    fn reasoning_round_trips_verbatim() {
        // The wire item — id, encrypted_content, summary — must re-emit
        // untouched; the harness never inspects it (invariant 6).
        let item = json!({
            "type": "reasoning",
            "id": "rs_abc",
            "encrypted_content": "ENCRYPTED-BLOB",
            "summary": [],
        });
        let m = Message {
            role: Role::Assistant,
            content: vec![Block::Reasoning { raw: item.clone() }],
        };
        let mut out = Vec::new();
        ir_message_to_input(&m, &Default::default(), &mut out);
        assert_eq!(out, vec![item]);
    }

    #[test]
    fn parse_output_items_and_usage() {
        let body = json!({
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "E"},
                {"type": "message", "role": "assistant",
                 "content": [{"type": "output_text", "text": "done"}]},
                {"type": "function_call", "id": "fc_1",
                 "call_id": "call_1", "name": "bash",
                 "arguments": "{\"command\":\"ls\"}"},
            ],
            "usage": {"input_tokens": 100, "output_tokens": 50,
                "input_tokens_details": {"cached_tokens": 60},
                "output_tokens_details": {"reasoning_tokens": 20}},
        });
        let r = ResponsesApi::parse_response(&body, 10, 5).unwrap();
        assert_eq!(r.stop_reason, StopReason::ToolUse);
        assert!(matches!(r.blocks[0], Block::Reasoning { .. }));
        assert!(matches!(r.blocks[1], Block::Text { .. }));
        match &r.blocks[2] {
            Block::ToolCall { id, name, input } => {
                assert_eq!(id, "call_1");
                assert_eq!(name, "bash");
                assert_eq!(input["command"], "ls");
            }
            _ => panic!("expected ToolCall"),
        }
        assert_eq!(r.usage.fresh_input, 40);
        assert_eq!(r.usage.cache_read, 60);
        assert_eq!(r.usage.output, 30); // 50 − 20 reasoning
        assert_eq!(r.usage.reasoning, 20);
    }

    #[test]
    fn incomplete_max_tokens_maps_stop_reason() {
        let body = json!({
            "status": "incomplete",
            "incomplete_details": {"reason": "max_output_tokens"},
            "output": [{"type": "message", "content":
                [{"type": "output_text", "text": "partial"}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1024},
        });
        let r = ResponsesApi::parse_response(&body, 1, 1).unwrap();
        assert_eq!(r.stop_reason, StopReason::MaxTokens);
    }

    #[test]
    fn effort_maps_to_reasoning_block() {
        let system: Vec<SystemSegment> = vec![];
        let tools: Vec<ToolSpec> = vec![];
        let msgs = vec![Message::user_text("hi")];
        let mut req = bare_req(&system, &tools, &msgs);
        req.effort = Some(Effort::Max);
        let body = ResponsesApi::build_body(&req);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        req.thinking_budget = Some(4096);
        req.effort = Some(Effort::Min);
        let body = ResponsesApi::build_body(&req);
        assert_eq!(body["reasoning"]["effort"], "high"); // budget wins
    }

    fn stop_of(body: Value) -> StopReason {
        ResponsesApi::parse_response(&body, 1, 1)
            .unwrap()
            .stop_reason
    }

    /// C4 (invariant 7): every status/reason maps explicitly; unknown
    /// values survive raw in `Other`.
    #[test]
    fn stop_reason_mapping_preserves_raw() {
        let inc = |reason: &str| {
            json!({"status": "incomplete",
                   "incomplete_details": {"reason": reason}, "output": []})
        };
        assert_eq!(
            stop_of(json!({"status": "completed", "output": []})),
            StopReason::EndTurn
        );
        assert_eq!(stop_of(inc("max_output_tokens")), StopReason::MaxTokens);
        assert_eq!(stop_of(inc("content_filter")), StopReason::Refusal);
        assert_eq!(
            stop_of(inc("max_tool_calls")),
            StopReason::Other("max_tool_calls".into())
        );
        for s in ["failed", "cancelled", "in_progress"] {
            assert_eq!(
                stop_of(json!({"status": s, "output": []})),
                StopReason::Other(s.into())
            );
        }
        // A tool call wins over any status.
        let mut with_call = inc("max_output_tokens");
        with_call["output"] = json!([{"type": "function_call", "call_id": "c",
                                      "name": "bash", "arguments": "{}"}]);
        assert_eq!(stop_of(with_call), StopReason::ToolUse);
    }

    /// C4: unparseable `arguments` are preserved raw, not nulled.
    #[test]
    fn unparseable_arguments_preserved_raw() {
        let body = json!({"status": "completed", "output": [
            {"type": "function_call", "call_id": "c1", "name": "write",
             "arguments": "{not json"}
        ]});
        let r = ResponsesApi::parse_response(&body, 1, 1).unwrap();
        match &r.blocks[0] {
            Block::ToolCall { input, .. } => {
                assert_eq!(input, &json!({"_unparsed": "{not json"}))
            }
            other => panic!("expected ToolCall, got {other:?}"),
        }
    }

    /// C3/C12: computer_call linkage lives here — a non-object action
    /// from the wire must not panic and survives under `_unparsed`.
    #[test]
    fn computer_call_parses_with_linkage_and_non_object_action() {
        for action in [json!({"type": "click", "x": 1}), json!("click"), json!([1])] {
            let body = json!({"id": "resp_7", "status": "completed", "output": [
                {"type": "computer_call", "id": "cu_1", "call_id": "call_cu",
                 "action": action.clone(),
                 "pending_safety_checks": [{"id": "s9"}]}
            ]});
            let r = ResponsesApi::parse_response(&body, 1, 1).unwrap();
            assert_eq!(r.stop_reason, StopReason::ToolUse);
            match &r.blocks[0] {
                Block::ToolCall { id, name, input } => {
                    assert_eq!(id, "call_cu");
                    assert_eq!(name, "computer");
                    assert_eq!(input["previous_response_id"], "resp_7");
                    assert_eq!(input["pending_safety_checks"][0]["id"], "s9");
                    assert!(!crate::provider::openai::safety_ack_complete(input));
                    if !action.is_object() {
                        assert_eq!(input["_unparsed"], action);
                    }
                }
                other => panic!("expected ToolCall, got {other:?}"),
            }
        }
    }

    /// C12: results are marked by the originating call's name, not by
    /// sniffing content — a plain result starting with "[computer]" stays
    /// a function_call_output.
    #[test]
    fn computer_result_keyed_by_call_name() {
        let msgs = vec![
            Message {
                role: Role::Assistant,
                content: vec![
                    Block::ToolCall {
                        id: "cu_1".into(),
                        name: "computer".into(),
                        input: json!({"type": "click"}),
                    },
                    Block::ToolCall {
                        id: "b_1".into(),
                        name: "bash".into(),
                        input: json!({"command": "echo"}),
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
                    content: "[computer] not really".into(),
                    is_error: false,
                },
            ]),
        ];
        let body = ResponsesApi::build_body(&bare_req(&[], &[], &msgs));
        let items = body["input"].as_array().unwrap();
        let find = |id: &str, ty: &str| items.iter().any(|i| i["call_id"] == id && i["type"] == ty);
        assert!(find("cu_1", "computer_call"));
        assert!(find("cu_1", "computer_call_output"));
        assert!(find("b_1", "function_call"));
        assert!(find("b_1", "function_call_output"));
    }

    /// K3: `input_tokens_details.cache_write_tokens` lands in cache_write
    /// (and, like cached_tokens, is carved out of input_tokens).
    #[test]
    fn cache_write_tokens_parsed() {
        let body = json!({"status": "completed", "output": [],
            "usage": {"input_tokens": 1000, "output_tokens": 5,
                "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300}}});
        let r = ResponsesApi::parse_response(&body, 1, 1).unwrap();
        assert_eq!(r.usage.cache_read, 600);
        assert_eq!(r.usage.cache_write, 300);
        assert_eq!(r.usage.fresh_input, 100);
    }
}
