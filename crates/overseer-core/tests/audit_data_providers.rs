//! Provider-adapter audit over a loopback mock server (the seam the adapter
//! unit tests use). Reproduced defects are `#[ignore = "audit: ..."]`.

use overseer_core::ir::{Block, Message, Role};
use overseer_core::provider::gemini::Gemini;
use overseer_core::provider::openai::OpenAiCompatible;
use overseer_core::provider::responses::ResponsesApi;
use overseer_core::provider::{Provider, ProviderError, Request, StopReason};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::thread::JoinHandle;

struct Reply {
    status: u16,
    headers: Vec<(&'static str, String)>,
    body: String,
}

fn ok(body: Value) -> Reply {
    Reply {
        status: 200,
        headers: vec![],
        body: body.to_string(),
    }
}

/// Serve `replies` one connection each; returns (base_url, captured
/// (head, body) per request).
fn serve(replies: Vec<Reply>) -> (String, JoinHandle<Vec<(String, String)>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let h = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for r in replies {
            let (mut sock, _) = listener.accept().unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let head_end = loop {
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break p + 4;
                }
                let n = sock.read(&mut chunk).unwrap();
                assert!(n > 0);
                buf.extend_from_slice(&chunk[..n]);
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let n = sock.read(&mut chunk).unwrap();
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
            }
            let body = String::from_utf8_lossy(&buf[head_end..]).to_string();
            let mut extra = String::new();
            for (k, v) in &r.headers {
                extra.push_str(&format!("{k}: {v}\r\n"));
            }
            let _ = write!(
                sock,
                "HTTP/1.1 {} X\r\ncontent-type: application/json\r\n{extra}content-length: {}\r\nconnection: close\r\n\r\n{}",
                r.status,
                r.body.len(),
                r.body
            );
            seen.push((head, body));
        }
        seen
    });
    (format!("http://127.0.0.1:{port}/v1"), h)
}

fn req<'a>(messages: &'a [Message]) -> Request<'a> {
    Request {
        model: "m",
        system: &[],
        tools: &[],
        messages,
        max_tokens: 64,
        thinking_budget: None,
        effort: None,
        cache_breakpoints: false,
        cache_key: None,
    }
}

fn assistant(blocks: Vec<Block>) -> Message {
    Message {
        role: Role::Assistant,
        content: blocks,
    }
}

fn result(id: &str, content: &str) -> Block {
    Block::ToolResult {
        tool_use_id: id.into(),
        content: content.into(),
        is_error: false,
    }
}

fn gemini_reply(parts: Value, finish: &str) -> Reply {
    ok(json!({
        "candidates": [{"content": {"role": "model", "parts": parts}, "finishReason": finish}],
        "usageMetadata": {"promptTokenCount": 3, "candidatesTokenCount": 2}
    }))
}

// ------------------------------------------------------------------ gemini

/// Invariant 6: Gemini puts `thoughtSignature` on the functionCall part of
/// a tool turn. The adapter keeps only name+args, so the signature is gone
/// when the turn is echoed back (Gemini 3 rejects / degrades unsigned
/// function-call turns).
#[test]
#[ignore = "audit: gemini-fc-signature"]
fn gemini_function_call_signature_roundtrips() {
    let first = gemini_reply(
        json!([{"functionCall": {"name": "read", "args": {"p": "a"}}, "thoughtSignature": "SIG-FC-123"}]),
        "STOP",
    );
    let second = gemini_reply(json!([{"text": "done"}]), "STOP");
    let (base, srv) = serve(vec![first, second]);
    let g = Gemini::new("k", base);
    let mut msgs = vec![Message::user_text("go")];
    let r = g.complete(&req(&msgs)).unwrap();
    assert_eq!(r.stop_reason, StopReason::ToolUse);
    msgs.push(assistant(r.blocks));
    msgs.push(Message::tool_results(vec![result("read", "A")]));
    g.complete(&req(&msgs)).unwrap();
    let seen = srv.join().unwrap();
    let body: Value = serde_json::from_str(&seen[1].1).unwrap();
    let echoed = body["contents"][1]["parts"].to_string();
    assert!(
        echoed.contains("SIG-FC-123"),
        "signature dropped on echo: {echoed}"
    );
}

/// Held: two parallel calls with the same name decode as two calls and the
/// results go back as two functionResponse parts, in call order.
#[test]
fn gemini_parallel_same_name_calls_pair_in_order() {
    let first = gemini_reply(
        json!([
            {"functionCall": {"name": "read", "args": {"p": "a"}}},
            {"functionCall": {"name": "read", "args": {"p": "b"}}}
        ]),
        "STOP",
    );
    let (base, srv) = serve(vec![first, gemini_reply(json!([{"text": "ok"}]), "STOP")]);
    let g = Gemini::new("k", base);
    let mut msgs = vec![Message::user_text("go")];
    let r = g.complete(&req(&msgs)).unwrap();
    let calls: Vec<Value> = r
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::ToolCall { input, .. } => Some(input["p"].clone()),
            _ => None,
        })
        .collect();
    assert_eq!(calls, vec![json!("a"), json!("b")]);
    msgs.push(assistant(r.blocks));
    msgs.push(Message::tool_results(vec![
        result("read", "AAA"),
        result("read", "BBB"),
    ]));
    g.complete(&req(&msgs)).unwrap();
    let seen = srv.join().unwrap();
    let body: Value = serde_json::from_str(&seen[1].1).unwrap();
    let parts = body["contents"][2]["parts"].as_array().unwrap().clone();
    assert_eq!(parts.len(), 2);
    let s0 = parts[0].to_string();
    let s1 = parts[1].to_string();
    assert!(s0.contains("AAA") && s1.contains("BBB"), "{s0} / {s1}");
}

/// Invariant 7: five distinct Gemini block reasons collapse into one
/// `Refusal`; the raw reason (SAFETY vs RECITATION vs SPII ...) is gone.
#[test]
#[ignore = "audit: gemini-stop-collapse"]
fn gemini_block_reasons_stay_distinguishable() {
    let reasons = [
        "SAFETY",
        "RECITATION",
        "BLOCKLIST",
        "SPII",
        "PROHIBITED_CONTENT",
    ];
    let (base, srv) = serve(
        reasons
            .iter()
            .map(|r| gemini_reply(json!([{"text": "x"}]), r))
            .collect(),
    );
    let g = Gemini::new("k", base);
    let msgs = vec![Message::user_text("go")];
    let got: Vec<String> = reasons
        .iter()
        .map(|_| {
            g.complete(&req(&msgs))
                .unwrap()
                .stop_reason
                .as_str()
                .to_string()
        })
        .collect();
    srv.join().unwrap();
    let mut uniq = got.clone();
    uniq.sort();
    uniq.dedup();
    assert_eq!(uniq.len(), reasons.len(), "raw reasons collapsed: {got:?}");
}

// --------------------------------------------------------------- responses

/// A Responses `refusal` content part is dropped: the turn comes back as
/// an empty EndTurn with no text, so the user sees nothing and the loop's
/// empty-response handling fires instead of a refusal.
#[test]
#[ignore = "audit: responses-refusal-drop"]
fn responses_refusal_part_is_surfaced() {
    let (base, srv) = serve(vec![ok(json!({
        "id": "r1", "status": "completed",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "refusal", "refusal": "I can't help with that."}]}],
        "usage": {"input_tokens": 5, "output_tokens": 5}
    }))]);
    let p = ResponsesApi::new("k", base);
    let msgs = vec![Message::user_text("go")];
    let r = p.complete(&req(&msgs)).unwrap();
    srv.join().unwrap();
    let text: String = r
        .blocks
        .iter()
        .filter_map(|b| match b {
            Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        text.contains("I can't help") || r.stop_reason == StopReason::Refusal,
        "refusal lost: blocks={:?} stop={:?}",
        r.blocks,
        r.stop_reason
    );
}

/// Held (invariant 6): a reasoning item (summary + encrypted_content) is
/// echoed back byte-identical; usage carves cached + cache_write out of
/// input and reasoning out of output; the session header reaches the wire.
#[test]
fn responses_reasoning_roundtrip_usage_and_header() {
    let reasoning = json!({"type": "reasoning", "id": "rs_1",
        "summary": [{"type": "summary_text", "text": "thinking ü"}],
        "encrypted_content": "gAAAA-opaque-blob=="});
    let (base, srv) = serve(vec![
        ok(json!({"id": "r1", "status": "completed",
            "output": [reasoning.clone(),
                {"type": "message", "content": [{"type": "output_text", "text": "hi"}]}],
            "usage": {"input_tokens": 1000, "output_tokens": 50,
                "input_tokens_details": {"cached_tokens": 600, "cache_write_tokens": 300},
                "output_tokens_details": {"reasoning_tokens": 20}}})),
        ok(json!({"id": "r2", "status": "completed", "output": [],
            "usage": {"input_tokens": 1, "output_tokens": 1}})),
    ]);
    let p = ResponsesApi::new("k", base).with_header("x-opencode-session", "audit-tag");
    let mut msgs = vec![Message::user_text("go")];
    let r = p.complete(&req(&msgs)).unwrap();
    assert_eq!(
        (
            r.usage.fresh_input,
            r.usage.cache_read,
            r.usage.cache_write,
            r.usage.output,
            r.usage.reasoning
        ),
        (100, 600, 300, 30, 20)
    );
    msgs.push(assistant(r.blocks));
    msgs.push(Message::user_text("again"));
    p.complete(&req(&msgs)).unwrap();
    let seen = srv.join().unwrap();
    assert!(seen.iter().all(|(h, _)| h
        .to_ascii_lowercase()
        .contains("x-opencode-session: audit-tag")));
    let body: Value = serde_json::from_str(&seen[1].1).unwrap();
    let items = body["input"].as_array().unwrap();
    assert!(
        items.iter().any(|i| i == &reasoning),
        "reasoning not echoed verbatim: {items:?}"
    );
}

// ------------------------------------------------------------------ openai

/// Held: OpenAI cached/reasoning and DeepSeek hit/miss accounting; unknown
/// finish_reason survives raw; truncated tool args survive raw.
#[test]
fn openai_usage_stop_and_truncated_args() {
    let (base, srv) = serve(vec![
        ok(
            json!({"choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 30,
                "prompt_tokens_details": {"cached_tokens": 80},
                "completion_tokens_details": {"reasoning_tokens": 10}}}),
        ),
        ok(
            json!({"choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": "insufficient_system_resource"}],
            "usage": {"prompt_tokens": 100, "completion_tokens": 5,
                "prompt_cache_hit_tokens": 64, "prompt_cache_miss_tokens": 36}}),
        ),
        ok(
            json!({"choices": [{"message": {"role": "assistant", "content": null,
                "tool_calls": [{"id": "c1", "type": "function",
                    "function": {"name": "write", "arguments": "{\"path\":\"a\",\"conte"}}]},
                "finish_reason": "length"}],
            "usage": {"prompt_tokens": 1, "completion_tokens": 64}}),
        ),
    ]);
    let p = OpenAiCompatible::new("k", base);
    let msgs = vec![Message::user_text("go")];
    let a = p.complete(&req(&msgs)).unwrap();
    assert_eq!(
        (
            a.usage.fresh_input,
            a.usage.cache_read,
            a.usage.output,
            a.usage.reasoning
        ),
        (20, 80, 20, 10)
    );
    let b = p.complete(&req(&msgs)).unwrap();
    assert_eq!((b.usage.fresh_input, b.usage.cache_read), (36, 64));
    assert_eq!(b.stop_reason.as_str(), "insufficient_system_resource");
    let c = p.complete(&req(&msgs)).unwrap();
    assert_eq!(c.stop_reason, StopReason::MaxTokens);
    let raw = c.blocks.iter().find_map(|b| match b {
        Block::ToolCall { input, .. } => input.get("_unparsed").cloned(),
        _ => None,
    });
    assert_eq!(raw, Some(json!("{\"path\":\"a\",\"conte")));
    srv.join().unwrap();
}

/// Held: 429/529 map to RateLimit with Retry-After seconds → ms (default
/// 5 s); 500 and context-length 400s map to Http with the body kept.
#[test]
fn openai_error_mapping() {
    let err = |status: u16, headers: Vec<(&'static str, String)>, body: &str| Reply {
        status,
        headers,
        body: body.into(),
    };
    let ctx = r#"{"error":{"code":"context_length_exceeded","message":"too long"}}"#;
    let (base, srv) = serve(vec![
        err(429, vec![("retry-after", "7".into())], "{}"),
        err(529, vec![], "{}"),
        err(500, vec![], r#"{"error":"boom"}"#),
        err(400, vec![], ctx),
    ]);
    let p = OpenAiCompatible::new("k", base);
    let msgs = vec![Message::user_text("go")];
    let r1 = p.complete(&req(&msgs)).unwrap_err();
    assert!(
        matches!(
            r1,
            ProviderError::RateLimit {
                status: 429,
                retry_after_ms: 7000
            }
        ),
        "{r1:?}"
    );
    let r2 = p.complete(&req(&msgs)).unwrap_err();
    assert!(
        matches!(
            r2,
            ProviderError::RateLimit {
                status: 529,
                retry_after_ms: 5000
            }
        ),
        "{r2:?}"
    );
    let r3 = p.complete(&req(&msgs)).unwrap_err();
    assert!(
        matches!(r3, ProviderError::Http { status: 500, .. }),
        "{r3:?}"
    );
    let r4 = p.complete(&req(&msgs)).unwrap_err();
    assert!(
        matches!(&r4, ProviderError::Http { status: 400, body } if body.contains("context_length_exceeded")),
        "{r4:?}"
    );
    srv.join().unwrap();
}

/// `Retry-After` is multiplied by 1000 unchecked: a hostile/buggy gateway
/// header panics the agent in debug builds and wraps to a bogus delay in
/// release (no overflow-checks in `[profile.release]`).
#[test]
#[ignore = "audit: provider-retry-after-overflow"]
fn huge_retry_after_does_not_panic() {
    let (base, srv) = serve(vec![Reply {
        status: 429,
        headers: vec![("retry-after", u64::MAX.to_string())],
        body: "{}".into(),
    }]);
    let p = OpenAiCompatible::new("k", base);
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let msgs = vec![Message::user_text("go")];
        p.complete(&req(&msgs)).map(|_| ())
    }));
    let _ = srv.join();
    let r = r.expect("complete() panicked on a Retry-After header");
    match r {
        Err(ProviderError::RateLimit { retry_after_ms, .. }) => {
            assert!(retry_after_ms >= 1000, "wrapped delay {retry_after_ms}")
        }
        other => panic!("{other:?}"),
    }
}
