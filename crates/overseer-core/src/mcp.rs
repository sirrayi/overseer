//! MCP server stdio client — the *client* half of the Model Context Protocol
//! (2025-06-18 revision, `stdio` transport), as implemented by
//! `modelcontextprotocol/servers` and the official `mcp` SDK clients.
//!
//! Nothing from those projects but the protocol client lands here: no server
//! is shipped, no runtime is linked, no network is touched and no model is
//! called. The engine spawns a program the operator names
//! ([`StdioClient::spawn`]) and speaks JSON-RPC 2.0 to it over that child's
//! own stdin/stdout. What the port contributes is the *shape*: framing, the
//! response reader, the handshake, the `tools/list` → tool-spec translation,
//! and the name-collision guard that stops a discovered tool from shadowing a
//! resident one.
//!
//! **Framing: newline-delimited JSON, never LSP `Content-Length`.** MCP's
//! stdio transport is one JSON-RPC message per line, UTF-8, no header block.
//! The LSP framing (`Content-Length: N\r\n\r\n{…}`) is the classic bug this
//! port must not repeat: an MCP server parses our first line as JSON and
//! rejects `Content-Length: 57`, and a client that expects a header block
//! reads the server's JSON as a headerless frame and desynchronizes on the
//! first message. Both directions here are line-oriented: [`request`] and
//! [`notification`] return exactly one line (a `\n` inside a string stays
//! escaped as `\n`, so it can never split a message in two), and
//! [`decode`] consumes exactly one.
//!
//! **Handshake order** (required by the protocol): `initialize` request →
//! server result → `notifications/initialized` notification → `tools/list` →
//! `tools/call`. [`StdioClient::initialize`] performs the first two steps
//! itself, because a caller who skips the notification leaves the server in a
//! state where it will not answer `tools/list`. MCP defines **no `shutdown`
//! method** (unlike LSP's `shutdown`/`exit` pair, which this client
//! deliberately does not send): the stdio transport ends by closing the
//! child's stdin and reaping it — see [`StdioClient::shutdown`].
//!
//! **Namespacing + the collision guard exist because MCP tool names are
//! chosen by third-party servers, not by us.** [`namespaced`] prefixes the
//! server id so two servers may both offer `read`, and
//! [`collides_with_resident`] refuses a name that would shadow a resident
//! tool (`bash`, `edit`, …). Shadowing a permission-gated resident tool with
//! an ungated remote one is a privilege escalation, not a naming preference.
//!
//! `// DEFERRED(owner): MCP revisions other than
//! [`PROTOCOL_VERSION`] (a server that answers with a different revision is
//! refused, per the spec's client rule for an unsupported version), MCP's
//! HTTP/SSE and streamable-HTTP transports plus every auth flow, server-
//! initiated messages (logging notifications, `sampling`/`roots`/
//! `elicitation` requests), which desynchronize the one-line-per-call reader
//! and need a demultiplexing reader task, and a server supervisor beyond
//! lazy spawn + drop-on-failure (health probes, restart policy, per-server
//! resource limits) — this batch lands the client, the framing, the
//! tool-spec translation, the name-collision guard and the `mcp` resident
//! tool (`tools::mcp_tool`) that drives the servers through the engine.`
//!
//! DONE, not deferred: per-call timeouts and the hung-server watchdog.
//! [`CALL_TIMEOUT`] bounds every [`StdioClient::call`];
//! [`StdioClient::call_with_timeout`] bounds one call, and a server with no
//! answer inside its timeout is killed, so a hung server cannot block
//! forever.

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};

use crate::provider::ToolSpec;

/// The MCP revision this client speaks; sent as `protocolVersion` in
/// `initialize` and required to be echoed back ([`StdioClient::initialize`]).
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// The `description` an MCP tool that declares none is registered with.
///
/// Deliberately a constant and not `"<name> (from <server>)"`: the same
/// `tools/list` fixture must translate to byte-identical specs on every
/// machine, and [`tool_specs`] is not handed a server id, so a name-bearing
/// description would have to be invented. A deterministic placeholder the
/// operator can spot in the tool list beats a fabricated sentence.
pub const DEFAULT_TOOL_DESCRIPTION: &str = "MCP tool";

/// The JSON-RPC 2.0 code reserved for a malformed request envelope
/// (`-32600 Invalid Request`). [`result_of`] reports an envelope that is
/// neither a result nor a well-formed error with this code, so a caller can
/// tell "the peer reported this" from "the peer's message was unreadable".
pub const CODE_INVALID_REQUEST: i64 = -32600;

/// How long [`StdioClient::shutdown`] lets a server exit after stdin closes
/// before it kills it. Fixed rather than configurable: shutdown is a
/// transport teardown, not a call — per-call liveness lives in
/// [`CALL_TIMEOUT`] and [`StdioClient::call_with_timeout`].
#[cfg(test)]
const SHUTDOWN_GRACE: Duration = Duration::from_millis(2_000);

/// How long [`StdioClient::call`] waits for one response line before it kills
/// the server and fails. A server that accepts a line and never answers must
/// not wedge the engine: the bound is fixed (not per-call configurable) so
/// every call site shares one liveness story; a caller that needs a different
/// bound uses [`StdioClient::call_with_timeout`] directly.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(30);

/// Poll interval for the shutdown and reap grace windows.
const SHUTDOWN_POLL: Duration = Duration::from_millis(10);

/// Longest wire excerpt an error message may embed, in chars (never bytes —
/// a cap must not split a multi-byte character).
const EXCERPT_CHARS: usize = 160;

/// One JSON-RPC 2.0 **request** as a single stdio line, without the trailing
/// newline the writer appends.
///
/// Invariants: `method` must be non-blank (a blank method is a caller bug and
/// no server can answer it); `params` must be `null`, an object or an array —
/// JSON-RPC's own constraint — where `null` means "omitted", so the emitted
/// line carries no `params` key at all. The returned line contains no raw
/// newline, because `serde_json` escapes control characters inside strings.
/// Key order is `serde_json`'s (its default map is ordered by key), so the
/// same request is byte-identical everywhere.
pub fn request(id: u64, method: &str, params: Value) -> Result<String, String> {
    build(method, params, Some(id))
}

/// One JSON-RPC 2.0 **notification** (a request with no `id`, expecting no
/// response) as a single stdio line, newline excluded.
///
/// Same invariants as [`request`] minus the id: MCP's
/// `notifications/initialized` carries no params, which is expressed by
/// passing `Value::Null` (JSON-RPC forbids a `null` params member, so
/// omitting it is the only faithful encoding).
pub fn notification(method: &str, params: Value) -> Result<String, String> {
    build(method, params, None)
}

/// Shared encoder behind [`request`] and [`notification`].
fn build(method: &str, params: Value, id: Option<u64>) -> Result<String, String> {
    if method.trim().is_empty() {
        return Err(format!(
            "a JSON-RPC method name must be a non-blank string, got {method:?}"
        ));
    }
    if !(params.is_null() || params.is_object() || params.is_array()) {
        return Err(format!(
            "JSON-RPC `params` must be an object or an array (or null to omit it), got {method} params {}",
            excerpt(&params.to_string())
        ));
    }
    let mut msg = Map::new();
    msg.insert("jsonrpc".to_string(), json!("2.0"));
    if let Some(id) = id {
        msg.insert("id".to_string(), json!(id));
    }
    msg.insert("method".to_string(), json!(method));
    if !params.is_null() {
        msg.insert("params".to_string(), params);
    }
    let line = serde_json::to_string(&Value::Object(msg))
        .map_err(|e| format!("could not encode the `{method}` message: {e}"))?;
    debug_assert!(
        !line.contains('\n'),
        "an encoded message must be one line: {line}"
    );
    Ok(line)
}

/// Parse one stdio line into a JSON-RPC 2.0 **response** envelope.
///
/// Invariants upheld: the line must be a JSON object carrying
/// `"jsonrpc":"2.0"` and an integer `id`, and it must carry *exactly one* of
/// `result` or `error` — both is a server bug (which one is the answer?) and
/// neither is not an answer at all, so both are refused rather than guessed.
///
/// This checks shape only. Matching the id against the request in flight
/// needs the caller's id and lives in [`decode_response`], which
/// [`StdioClient::call`] uses; never accept a response on [`decode`] alone,
/// or a stale line from an earlier request is read as this one's answer.
pub fn decode(line: &str) -> Result<Value, String> {
    let msg: Value = serde_json::from_str(line).map_err(|e| {
        format!(
            "stdio line is not JSON: {e} (line: {})",
            excerpt(line.trim_end())
        )
    })?;
    let obj = match msg.as_object() {
        Some(obj) => obj,
        None => {
            return Err(format!(
                "a JSON-RPC response must be an object, got {}",
                excerpt(&msg.to_string())
            ))
        }
    };
    match obj.get("jsonrpc") {
        Some(Value::String(v)) if v == "2.0" => {}
        other => {
            return Err(format!(
                "a JSON-RPC response must carry \"jsonrpc\":\"2.0\", got {}",
                other.map_or("no `jsonrpc` member".to_string(), |v| v.to_string())
            ))
        }
    }
    match obj.get("id") {
        Some(Value::Number(n)) if n.is_i64() || n.is_u64() => {}
        other => {
            return Err(format!(
                "a JSON-RPC response `id` must be an integer, got {}",
                other.map_or("no `id` member".to_string(), |v| v.to_string())
            ))
        }
    }
    match (obj.contains_key("result"), obj.contains_key("error")) {
        (true, true) => Err(
            "a JSON-RPC response must carry exactly one of `result`/`error`, but carries both"
                .to_string(),
        ),
        (false, false) => Err(
            "a JSON-RPC response must carry exactly one of `result`/`error`, but carries neither"
                .to_string(),
        ),
        _ => Ok(msg),
    }
}

/// [`decode`] plus the id check: the response's `id` must equal
/// `expected_id`.
///
/// A mismatched id means stdout carried a line belonging to some other
/// request (a late answer to a timed-out call, or a server pushing a message
/// in the middle of a call). It is an error naming both ids — silently
/// accepting it would hand one request's answer to another.
pub fn decode_response(line: &str, expected_id: u64) -> Result<Value, String> {
    let msg = decode(line)?;
    // `decode` guarantees an integer `id`; a negative one can never match a
    // u64 request id and is reported as the mismatch it is.
    if msg.get("id").and_then(Value::as_u64) == Some(expected_id) {
        Ok(msg)
    } else {
        Err(format!(
            "response `id` {} does not match the pending request id {expected_id}",
            msg.get("id").map_or("null".to_string(), |v| v.to_string())
        ))
    }
}

/// A JSON-RPC 2.0 error object as the peer sent it: `code` and `message` are
/// both required by the protocol, so a missing one is an unreadable error
/// rather than a silent default (see [`result_of`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpcError {
    /// The peer's error code, e.g. `-32601` (method not found).
    pub code: i64,
    /// The peer's human-readable message. Always non-empty: an error object
    /// without one is reported as [`CODE_INVALID_REQUEST`] with an explanation
    /// instead of an empty string.
    pub message: String,
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "json-rpc error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for RpcError {}

/// Read a decoded response envelope as its `result`.
///
/// Invariants: an `error` member yields `Err(RpcError)` carrying the peer's
/// code and message (a missing or non-string `message`, or a non-integer
/// `code`, makes the error unreadable and is reported as
/// [`CODE_INVALID_REQUEST`] with the offending value named). An envelope with
/// neither member, or with both, is a malformed response and is also reported
/// as [`CODE_INVALID_REQUEST`] — never as a successful `null`.
pub fn result_of(msg: &Value) -> Result<&Value, RpcError> {
    let obj = msg.as_object().ok_or_else(|| RpcError {
        code: CODE_INVALID_REQUEST,
        message: format!(
            "a JSON-RPC message must be an object, got {}",
            excerpt(&msg.to_string())
        ),
    })?;
    if let Some(err) = obj.get("error") {
        if obj.contains_key("result") {
            return Err(RpcError {
                code: CODE_INVALID_REQUEST,
                message: "response carries both `result` and `error`; exactly one is required"
                    .to_string(),
            });
        }
        return Err(error_of(err));
    }
    obj.get("result").ok_or_else(|| RpcError {
        code: CODE_INVALID_REQUEST,
        message: "response carries neither `result` nor `error`; exactly one is required"
            .to_string(),
    })
}

/// Read the `error` member into an [`RpcError`], naming whatever is wrong
/// with it.
fn error_of(err: &Value) -> RpcError {
    let unreadable = |detail: String| RpcError {
        code: CODE_INVALID_REQUEST,
        message: detail,
    };
    let Some(obj) = err.as_object() else {
        return unreadable(format!(
            "a JSON-RPC `error` must be an object, got {}",
            excerpt(&err.to_string())
        ));
    };
    let Some(code) = obj.get("code").and_then(Value::as_i64) else {
        return unreadable(format!(
            "a JSON-RPC `error` must carry an integer `code`, got {}",
            obj.get("code")
                .map_or("no `code` member".to_string(), |v| v.to_string())
        ));
    };
    let Some(message) = obj.get("message").and_then(Value::as_str) else {
        return unreadable(format!(
            "JSON-RPC error {code} must carry a string `message` (required by the protocol), got {}",
            obj.get("message")
                .map_or("no `message` member".to_string(), |v| v.to_string())
        ));
    };
    RpcError {
        code,
        message: message.to_string(),
    }
}

/// Translate the **result** of an MCP `tools/list` call
/// (`{"tools":[{name, description?, inputSchema?}, …]}`) into the engine's
/// [`ToolSpec`]s, in the order the server listed them (order is the server's
/// choice and is preserved — a caller that wants a canonical order sorts).
///
/// Invariants: `tools` must be an array and every entry an object; `name` is
/// required and must be non-empty; `description` defaults to
/// [`DEFAULT_TOOL_DESCRIPTION`] when absent or `null`; `inputSchema` defaults
/// to `{"type":"object","properties":{}}` when absent or `null` — the MCP
/// schema's own empty-object shape, i.e. "this tool takes no arguments" (an
/// absent schema must not become a permissive one that would let an
/// unvalidated argument object through). Any other type, or a malformed
/// entry, is refused with the offending entry index in the message, so a
/// broken fixture is a one-line fix rather than a hunt through the list.
pub fn tool_specs(list_result: &Value) -> Result<Vec<ToolSpec>, String> {
    let tools = match list_result.get("tools") {
        Some(Value::Array(tools)) => tools,
        Some(other) => {
            return Err(format!(
                "`tools/list` result member `tools` must be an array, got {}",
                excerpt(&other.to_string())
            ))
        }
        None => {
            return Err(format!(
                "`tools/list` result has no `tools` array: {}",
                excerpt(&list_result.to_string())
            ))
        }
    };
    let mut specs = Vec::with_capacity(tools.len());
    for (idx, entry) in tools.iter().enumerate() {
        let Some(obj) = entry.as_object() else {
            return Err(format!(
                "`tools/list` entry #{idx} must be an object, got {}",
                excerpt(&entry.to_string())
            ));
        };
        let name = match obj.get("name") {
            Some(Value::String(name)) if !name.trim().is_empty() => name.clone(),
            Some(other) => {
                return Err(format!(
                    "`tools/list` entry #{idx} member `name` must be a non-empty string, got {}",
                    excerpt(&other.to_string())
                ))
            }
            None => {
                return Err(format!(
                    "`tools/list` entry #{idx} has no `name` member (required): {}",
                    excerpt(&entry.to_string())
                ))
            }
        };
        let description = match obj.get("description") {
            None | Some(Value::Null) => DEFAULT_TOOL_DESCRIPTION.to_string(),
            Some(Value::String(s)) => s.clone(),
            Some(other) => {
                return Err(format!(
                    "`tools/list` entry #{idx} (`{name}`) member `description` must be a string, got {}",
                    excerpt(&other.to_string())
                ))
            }
        };
        let input_schema = match obj.get("inputSchema") {
            None | Some(Value::Null) => empty_object_schema(),
            Some(schema @ Value::Object(_)) => schema.clone(),
            Some(other) => {
                return Err(format!(
                    "`tools/list` entry #{idx} (`{name}`) member `inputSchema` must be an object, got {}",
                    excerpt(&other.to_string())
                ))
            }
        };
        specs.push(ToolSpec {
            name,
            description,
            input_schema,
        });
    }
    Ok(specs)
}

/// The MCP empty-object input schema: a tool that declares no `inputSchema`
/// takes no arguments.
fn empty_object_schema() -> Value {
    json!({ "type": "object", "properties": {} })
}

/// The registered name of an MCP tool: `mcp__<server>__<tool>`, both parts
/// sanitized (see [`sanitized`]).
///
/// Invariants: total — never fails and never yields a blank part (a part that
/// sanitizes to nothing becomes `_`), so the result is always a usable name;
/// characters outside `[A-Za-z0-9_]` become `_` one char at a time, so a
/// multi-byte character costs one `_` and the output stays ASCII. The caller
/// still validates the result (and should use [`collides_with_resident`],
/// which is the part that matters).
pub fn namespaced(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitized(server), sanitized(tool))
}

/// Map every character outside `[A-Za-z0-9_]` to `_`, char by char, and
/// collapse a blank result to `_`.
///
/// Public because it is the inverse direction a caller needs: given a full
/// [`namespaced`] name, the server segment is the sanitized server id, and
/// `tools::mcp_tool` matches that segment back to a configured server.
pub fn sanitized(part: &str) -> String {
    let mut out: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.is_empty() {
        out.push('_');
    }
    out
}

/// Whether registering an MCP tool named `tool` from `server` would shadow a
/// resident tool in `crate::tools::TOOL_NAMES`.
///
/// Three forms are compared, because any of them can end up in the registry:
/// the namespaced name ([`namespaced`]), the bare name as the server sent it,
/// and the sanitized bare name (a caller that drops the prefix registers the
/// sanitized form, so `"repo map"` must be caught as much as `"repo_map"`).
/// The comparison ignores ASCII case: the registry's names are lowercase and
/// a server is free to hand us `Read`, which the model would not distinguish
/// from `read`.
///
/// A false positive only forces a rename; a false negative lets an
/// ungated remote tool stand in for a permission-gated resident one, so the
/// test errs towards the collision.
pub fn collides_with_resident(server: &str, tool: &str) -> bool {
    let prefixed = namespaced(server, tool);
    let bare = sanitized(tool);
    crate::tools::TOOL_NAMES.iter().any(|resident| {
        resident.eq_ignore_ascii_case(&prefixed)
            || resident.eq_ignore_ascii_case(tool)
            || resident.eq_ignore_ascii_case(&bare)
    })
}

/// Rank `specs` against `query`, best first, deterministically.
///
/// Scoring per spec, first hit wins: an exact name match (ASCII
/// case-insensitive) scores `1.0`; a substring match of the folded query
/// in the folded name scores `0.7`; otherwise the fraction of folded
/// query tokens present in the folded `name + description` token set
/// (tokens split on non-alphanumerics). Specs scoring nothing are
/// dropped. Output is sorted score-descending, name-ascending, and
/// capped at `top_k`. A blank query or `top_k == 0` yields nothing.
/// Pure function of its inputs: no I/O, no clock, no randomness.
pub fn search_tools(specs: &[ToolSpec], query: &str, top_k: usize) -> Vec<(String, f32)> {
    if top_k == 0 {
        return Vec::new();
    }
    let q = query.trim().to_lowercase();
    if q.is_empty() {
        return Vec::new();
    }
    let q_tokens: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .collect();
    let mut scored: Vec<(String, f32)> = Vec::new();
    for spec in specs {
        let name_fold = spec.name.to_lowercase();
        let score = if name_fold == q {
            1.0
        } else if name_fold.contains(&q) {
            0.7
        } else if q_tokens.is_empty() {
            continue;
        } else {
            let hay_owned = format!("{} {}", spec.name, spec.description).to_lowercase();
            let hay: Vec<&str> = hay_owned
                .split(|c: char| !c.is_alphanumeric())
                .filter(|t| !t.is_empty())
                .collect();
            let hits = q_tokens.iter().filter(|t| hay.contains(t)).count();
            if hits == 0 {
                continue;
            }
            hits as f32 / q_tokens.len() as f32
        };
        scored.push((spec.name.clone(), score));
    }
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });
    scored.truncate(top_k);
    scored
}

/// Head-truncate a tool `description` to `cap` chars (never bytes — a cap
/// must not split a multi-byte character), marking the cut with `…`.
/// `cap == 0` yields empty; a description within `cap` returns whole.
pub fn trim_description(desc: &str, cap: usize) -> String {
    if cap == 0 {
        return String::new();
    }
    if desc.chars().count() <= cap {
        return desc.to_string();
    }
    let mut out: String = desc.chars().take(cap).collect();
    out.push('…');
    out
}

/// Cap on the shared stderr tail (bytes, kept tail-end so the freshest
/// diagnostics survive).
const STDERR_TAIL_CAP: usize = 2 * 1024;

/// Pump a child's stderr until EOF, keeping only the newest
/// [`STDERR_TAIL_CAP`] bytes of UTF-8-lossy text in the shared buffer.
/// Runs for the child's whole life on its own thread — the pipe can
/// never fill, so a logging-heavy server cannot deadlock the client.
fn drain_stderr(mut pipe: impl Read, tail: Arc<Mutex<String>>) {
    let mut buf = [0u8; 1024];
    loop {
        match pipe.read(&mut buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => {
                let chunk = String::from_utf8_lossy(&buf[..n]);
                let Ok(mut t) = tail.lock() else { return };
                t.push_str(&chunk);
                if t.len() > STDERR_TAIL_CAP {
                    let mut start = t.len() - STDERR_TAIL_CAP;
                    while !t.is_char_boundary(start) {
                        start += 1;
                    }
                    t.drain(..start);
                }
            }
        }
    }
}

/// A live MCP server process speaking newline-delimited JSON-RPC on stdio.
///
/// Invariants: stdout is read one line per request and every response's `id`
/// must match the request just written ([`decode_response`]) — a stale or
/// interleaved line is an error, never accepted as this call's answer; ids
/// start at 1 and are never reused, even when a write fails, so a late reply
/// to a failed call can never be mistaken for a fresh one; `stderr` is drained by a
/// daemon thread into a bounded (≤2 KiB) tail, so a chatty server cannot fill
/// a pipe buffer and deadlock the client while we wait on stdout — and a
/// crash still leaves a diagnostic crumb ([`StdioClient::stderr_tail`]); a
/// child that exits (or closes stdout)
/// is reported as an error naming the server and the method — a hang is never
/// papered over as an empty success; a server that never answers is killed
/// after [`CALL_TIMEOUT`] (see [`StdioClient::call_with_timeout`]).
/// Dropping the client kills and reaps the
/// child, so no path leaks a server process.
///
/// The client reads only its own responses: a server-initiated message
/// arriving between the request and the answer would desynchronize it, which
/// is why those are deferred (module header).
pub struct StdioClient {
    /// `None` once a timed-out child was handed to a background reaper.
    child: Option<Child>,
    /// `None` once shutdown (or a timeout kill) has closed it; any later call
    /// then fails naming that state instead of writing to a dead pipe.
    stdin: Option<ChildStdin>,
    /// `None` while a reader thread owns the pipe mid-call, and `None`
    /// forever after a timed-out call killed the server (stdin is closed with
    /// it, so the next call still fails on the shutdown state first).
    stdout: Option<BufReader<ChildStdout>>,
    next_id: u64,
    server: String,
    /// The ≤2 KiB most recent stderr bytes — kept by the drainer thread so
    /// spawn/init/transport failures can name what the server said.
    stderr_tail: Arc<Mutex<String>>,
}

impl StdioClient {
    /// Spawn an MCP server program and wire its stdio: our writes go to its
    /// stdin, its stdout is read line by line, its stderr is drained into a
    /// bounded tail ([`StdioClient::stderr_tail`]).
    ///
    /// `server` is the short id this server is registered under (used in
    /// errors and by [`namespaced`]); the program and its arguments are the
    /// operator's, taken verbatim — the client ships no server and never
    /// guesses one. Both names must be non-blank; a spawn failure names the
    /// server, the program and the OS error, so a missing binary is one line
    /// to diagnose. This form **inherits the parent environment**, so it is
    /// test-only: production spawns through
    /// [`spawn_with_env`](Self::spawn_with_env), the allowlisted path.
    #[cfg(test)]
    pub fn spawn(server: &str, program: &str, args: &[String]) -> Result<Self, String> {
        Self::spawn_inner(server, program, args, None)
    }

    /// Spawn an MCP server program (non-blank `server` id and `program`,
    /// verbatim operator args) with an explicit child environment instead of
    /// the inherited one: the child gets a **cleared** environment holding
    /// only `PATH` and `HOME` (taken from the parent when they are set) plus
    /// the `env` pairs the operator declared in the server's config.
    ///
    /// This is the engine's default spawn path, and the allowlist is the
    /// point: a third-party MCP server is untrusted code, so it must not
    /// inherit whatever the operator's shell happens to export — provider
    /// API keys (`ANTHROPIC_API_KEY`), broker secrets, CI tokens. A secret a
    /// server genuinely needs is declared in its config and pulled from the
    /// parent env there (`${VAR}` expansion in `mcp_config`), which makes the
    /// grant visible in the file instead of implicit. `PATH`/`HOME` ride
    /// along because a server that cannot find its own interpreter, or a
    /// home for its cache, is not a hardened server — just a broken one.
    /// Declared pairs win over the inherited two.
    pub fn spawn_with_env(
        server: &str,
        program: &str,
        args: &[String],
        env: &[(String, String)],
    ) -> Result<Self, String> {
        Self::spawn_inner(server, program, args, Some(env))
    }

    /// Shared spawn: `env: None` inherits the parent environment (the
    /// test-only `spawn`), `env: Some` is
    /// the cleared-environment path described on
    /// [`spawn_with_env`](Self::spawn_with_env).
    fn spawn_inner(
        server: &str,
        program: &str,
        args: &[String],
        env: Option<&[(String, String)]>,
    ) -> Result<Self, String> {
        if server.trim().is_empty() {
            return Err(format!(
                "an MCP server id must be a non-blank name (it namespaces the server's tools), got {server:?}"
            ));
        }
        if program.trim().is_empty() {
            return Err(format!(
                "mcp server `{server}`: the program to spawn must be a non-blank path, got {program:?}"
            ));
        }
        let mut command = Command::new(program);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(pairs) = env {
            command.env_clear();
            for key in ["PATH", "HOME"] {
                if let Some(value) = std::env::var_os(key) {
                    command.env(key, value);
                }
            }
            for (key, value) in pairs {
                command.env(key, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("mcp server `{server}`: could not spawn `{program}`: {e}"))?;
        let stdin = child.stdin.take().ok_or_else(|| {
            format!("mcp server `{server}`: `{program}` did not give us a stdin pipe")
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            format!("mcp server `{server}`: `{program}` did not give us a stdout pipe")
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            format!("mcp server `{server}`: `{program}` did not give us a stderr pipe")
        })?;
        // The drainer owns the pipe for the child's whole life and keeps
        // only the newest ≤2 KiB — the tail, not the stream, is bounded.
        let stderr_tail = Arc::new(Mutex::new(String::new()));
        std::thread::spawn({
            let tail = Arc::clone(&stderr_tail);
            move || drain_stderr(stderr, tail)
        });
        Ok(Self {
            child: Some(child),
            stdin: Some(stdin),
            stdout: Some(BufReader::new(stdout)),
            next_id: 1,
            server: server.to_string(),
            stderr_tail,
        })
    }

    /// The last ≤2 KiB the server wrote to stderr — empty when it stayed
    /// quiet. Diagnostics only (appended to spawn/init errors); never
    /// parsed for protocol state.
    pub fn stderr_tail(&self) -> String {
        self.stderr_tail
            .lock()
            .map(|t| t.clone())
            .unwrap_or_default()
    }

    /// Send one request and return the **decoded response envelope** (which
    /// still carries `result`/`error` — read it with [`result_of`]).
    ///
    /// One line out, one line back: the request is written and flushed as a
    /// single line, then exactly one stdout line is read and must decode as a
    /// response whose `id` is this call's. A closed stdout (the child exited)
    /// is an error naming the server, the method and the id — never a
    /// fabricated result, and never a wait that outlives the process.
    ///
    /// A well-formed *error* response is returned as `Ok`: at this layer it is
    /// a protocol answer, and the wrappers ([`initialize`](Self::initialize),
    /// [`list_tools`](Self::list_tools),
    /// [`call_tool_with_timeout`](Self::call_tool_with_timeout)) turn
    /// it into an `Err` with the server's own code and message.
    ///
    /// Bounded by [`CALL_TIMEOUT`]: this delegates to
    /// [`call_with_timeout`](Self::call_with_timeout), so a hung server is
    /// killed rather than waited on forever.
    pub fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.call_with_timeout(method, params, CALL_TIMEOUT)
    }

    /// [`call`](Self::call) with an explicit bound: the response must arrive
    /// within `timeout`, else the child is killed, reaped, and the call fails
    /// with an error naming the server, the method, the id and the timeout —
    /// plus `restart the server`, because the session is dead: stdin is closed
    /// with it, so the next call fails naming that shutdown state instead of
    /// writing to a dead pipe.
    ///
    /// The bound is enforced by a one-shot reader thread plus `recv_timeout`:
    /// the blocking `read_line` runs off-thread, so no path blocks the caller
    /// past `timeout`. The thread hands the reader back on success; on timeout
    /// the kill closes the pipe, which releases the stranded reader (its send
    /// then lands on a dropped receiver and is ignored). Ids burn exactly as
    /// documented on the struct: the id is consumed before the write, even
    /// when the call fails.
    pub fn call_with_timeout(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id;
        let line = request(id, method, params)?;
        // The id is burned even if the write fails: ids are never reused.
        self.next_id = self.next_id.saturating_add(1);
        self.write_line(&line, method)?;
        // The blocking read moves off-thread: a server that never answers
        // must not hold this call past `timeout`.
        let reader = self.stdout.take().ok_or_else(|| {
            format!(
                "mcp server `{}`: stdout is already closed (an earlier call timed out and the server was killed), cannot read the response to `{method}` (id {id})",
                self.server
            )
        })?;
        let (tx, rx) = mpsc::channel::<(BufReader<ChildStdout>, std::io::Result<usize>, String)>();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut buf = String::new();
            let read = reader.read_line(&mut buf);
            let _ = tx.send((reader, read, buf));
        });
        let (reader, read, buf) = match rx.recv_timeout(timeout) {
            Ok(out) => out,
            Err(_) => {
                // Hung server: kill it and reap it within REAP_GRACE (a child
                // stuck past SIGKILL is handed to a background reaper rather
                // than blocking this call), then close stdin so the next
                // call fails naming the shutdown state. `stdout` stays
                // `None`: the stranded reader thread still owns it and exits
                // once the kill closes the pipe.
                self.child = self.child.take().and_then(kill_and_reap);
                self.stdin.take();
                return Err(format!(
                    "mcp server `{}`: call timeout — no response to `{method}` (id {id}) within {} ms; the hung server was killed, restart the server",
                    self.server,
                    timeout.as_millis()
                ));
            }
        };
        self.stdout = Some(reader);
        let read = read.map_err(|e| {
            format!(
                "mcp server `{}`: reading the response to `{method}` (id {id}) failed: {e}",
                self.server
            )
        })?;
        if read == 0 {
            return Err(format!(
                "mcp server `{}`: exited early — stdout closed while waiting for the response to `{method}` (id {id}), so there is no result; restart the server",
                self.server
            ));
        }
        decode_response(&buf, id).map_err(|e| {
            format!(
                "mcp server `{}`: bad response to `{method}` (id {id}): {e}",
                self.server
            )
        })
    }

    /// The MCP handshake: `initialize`, then the required
    /// `notifications/initialized` notification, returning the server's
    /// `initialize` result (`{protocolVersion, capabilities, serverInfo}`).
    ///
    /// Invariants: the notification is sent only after a successful result
    /// (an error means there is no session to initialize) and is never
    /// skippable by the caller — an uninitialized server answers
    /// `tools/list` with an error. The result must be an object naming the
    /// same `protocolVersion` we speak: a server answering with another
    /// revision is talking a protocol this client does not implement, so it
    /// is refused by naming both revisions rather than being talked to
    /// anyway. `clientInfo` carries the caller's name and version, which
    /// servers log and some gate behaviour on.
    pub fn initialize(&mut self, client_name: &str, client_version: &str) -> Result<Value, String> {
        let params = json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": { "name": client_name, "version": client_version },
        });
        let msg = self.call("initialize", params)?;
        let result = result_of(&msg).map_err(|e| self.rpc_error("initialize", e))?;
        if !result.is_object() {
            return Err(format!(
                "mcp server `{}`: `initialize` result must be an object, got {}",
                self.server,
                excerpt(&result.to_string())
            ));
        }
        let version = result.get("protocolVersion").and_then(Value::as_str);
        if version != Some(PROTOCOL_VERSION) {
            return Err(format!(
                "mcp server `{}`: `initialize` answered protocolVersion {} but this client speaks {PROTOCOL_VERSION} only",
                self.server,
                version.map_or("(none)".to_string(), |v| format!("{v:?}"))
            ));
        }
        let line = notification("notifications/initialized", Value::Null)?;
        self.write_line(&line, "notifications/initialized")?;
        Ok(result.clone())
    }

    /// `tools/list`, translated to [`ToolSpec`]s by [`tool_specs`].
    pub fn list_tools(&mut self) -> Result<Vec<ToolSpec>, String> {
        let msg = self.call("tools/list", json!({}))?;
        let result = result_of(&msg).map_err(|e| self.rpc_error("tools/list", e))?;
        tool_specs(result)
    }

    /// `tools/call` for `tool` with `arguments` (MCP requires the arguments
    /// member even when the tool takes none, so `json!({})` is passed for a
    /// no-argument tool).
    ///
    /// The result is returned verbatim, including MCP's `isError: true`
    /// marker: that marker reports a *tool-level* outcome (the remote command
    /// failed, a file was missing) and is not a transport failure, so turning
    /// it into an `Err` here would erase MCP's own distinction between "the
    /// call did not happen" and "the call happened and the tool said no".
    /// Only a JSON-RPC `error` (no result at all) becomes an `Err`.
    #[cfg(test)]
    pub fn call_tool(&mut self, tool: &str, args: Value) -> Result<Value, String> {
        self.call_tool_with_timeout(tool, args, CALL_TIMEOUT)
    }

    /// `tools/call` with an explicit bound (the test-only `call_tool` uses
    /// [`CALL_TIMEOUT`]): identical
    /// request envelope and identical result mapping, but the response must
    /// arrive within `timeout` (see
    /// [`call_with_timeout`](Self::call_with_timeout), which enforces it —
    /// a server with no answer is killed, and the next call fails naming
    /// that shutdown state).
    ///
    /// The registry's `mcp` tool carries the bound so a test can drive the
    /// timeout path quickly instead of waiting out the fixed
    /// [`CALL_TIMEOUT`]; the shipped default is that same constant.
    pub fn call_tool_with_timeout(
        &mut self,
        tool: &str,
        args: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let msg = self.call_with_timeout(
            "tools/call",
            json!({ "name": tool, "arguments": args }),
            timeout,
        )?;
        let result =
            result_of(&msg).map_err(|e| self.rpc_error(&format!("tools/call of `{tool}`"), e))?;
        Ok(result.clone())
    }

    /// End the session: close the server's stdin and reap the child.
    ///
    /// MCP (2025-06-18) defines **no `shutdown` RPC method**, unlike LSP:
    /// sending LSP's `shutdown` here would earn a `-32601` (method not found),
    /// so the transport's own termination — stdin EOF — is what this does. The
    /// child then gets [`SHUTDOWN_GRACE`] to exit; a server still running when
    /// that window closes is killed, reaped, and reported as an `Err` naming
    /// it, because "shut down" must not mean "abandoned but alive". The exit
    /// status of a server that does exit is not judged (its stderr went to
    /// `/dev/null`, so a non-zero status here would be an unexplained claim).
    ///
    /// Calling it twice is a caller bug and says so rather than pretending a
    /// second shutdown happened.
    #[cfg(test)]
    pub fn shutdown(&mut self) -> Result<(), String> {
        if self.stdin.take().is_none() {
            return Err(format!(
                "mcp server `{}`: already shut down — stdin was closed earlier, so the server is gone (or was killed) and this session cannot be reused",
                self.server
            ));
        }
        let deadline = Instant::now() + SHUTDOWN_GRACE;
        loop {
            let Some(child) = self.child.as_mut() else {
                return Ok(());
            };
            match child.try_wait() {
                Ok(Some(_status)) => return Ok(()),
                Ok(None) => {}
                Err(e) => {
                    return Err(format!(
                        "mcp server `{}`: could not reap the child at shutdown: {e}",
                        self.server
                    ))
                }
            }
            if Instant::now() >= deadline {
                self.child = self.child.take().and_then(kill_and_reap);
                return Err(format!(
                    "mcp server `{}`: still running {} ms after stdin closed and had to be killed",
                    self.server,
                    SHUTDOWN_GRACE.as_millis()
                ));
            }
            std::thread::sleep(SHUTDOWN_POLL);
        }
    }

    /// The server id this client was spawned under.
    #[cfg(test)]
    pub fn server(&self) -> &str {
        &self.server
    }

    /// Write one already-framed line plus its newline, in one syscall.
    fn write_line(&mut self, line: &str, method: &str) -> Result<(), String> {
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            format!(
                "mcp server `{}`: stdin is already closed (the client was shut down), cannot send `{method}`",
                self.server
            )
        })?;
        let mut framed = String::with_capacity(line.len() + 1);
        framed.push_str(line);
        framed.push('\n');
        stdin
            .write_all(framed.as_bytes())
            .and_then(|()| stdin.flush())
            .map_err(|e| {
                format!(
                    "mcp server `{}`: writing the `{method}` line failed: {e}",
                    self.server
                )
            })
    }

    /// Prefix a JSON-RPC error with the server and method it came from.
    fn rpc_error(&self, what: &str, err: RpcError) -> String {
        format!("mcp server `{}`: `{what}` failed: {err}", self.server)
    }
}

/// A `Debug` view that names the server, its pid and the id counter, without
/// printing the pipe handles: a client in a log line should say which server
/// it is and whether its session is still open, nothing more.
impl std::fmt::Debug for StdioClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StdioClient")
            .field("server", &self.server)
            .field("pid", &self.child.as_ref().map(Child::id))
            .field("next_id", &self.next_id)
            .field("stdin_open", &self.stdin.is_some())
            .finish()
    }
}

impl Drop for StdioClient {
    /// Dropping the client kills and reaps the child: a server we spawned
    /// must not outlive the client that speaks its protocol, and nothing else
    /// in this module can be relied on to have run. On an already-reaped
    /// child the kill fails harmlessly and is ignored.
    fn drop(&mut self) {
        if let Some(child) = self.child.take() {
            kill_and_reap(child);
        }
    }
}

/// Bound on reaping a killed server before the reap moves off-thread.
const REAP_GRACE: Duration = Duration::from_millis(2_000);

/// SIGKILL `child`, then [`reap_or_detach`] it within [`REAP_GRACE`].
fn kill_and_reap(mut child: Child) -> Option<Child> {
    let _ = child.kill();
    reap_or_detach(child, REAP_GRACE)
}

/// Poll `try_wait` for up to `grace`. An exited child comes back (reaped);
/// one still alive — e.g. stuck in uninterruptible sleep past SIGKILL — is
/// moved to a background thread that blocks on `wait`, so no caller ever
/// does, and `None` is returned.
fn reap_or_detach(mut child: Child, grace: Duration) -> Option<Child> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return Some(child),
            Ok(None) if Instant::now() >= deadline => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
                return None;
            }
            Ok(None) => std::thread::sleep(SHUTDOWN_POLL),
        }
    }
}

/// A bounded, char-based excerpt of wire text for error messages: a server
/// that answers with megabytes must not produce a megabyte-long error, and a
/// cap must never split a multi-byte character.
fn excerpt(text: &str) -> String {
    let mut out: String = text.chars().take(EXCERPT_CHARS).collect();
    if text.chars().count() > EXCERPT_CHARS {
        out.push('…');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Spawn `<script>` under POSIX `sh`. The scripts below are stub servers:
    /// they read the lines we send and print one-line JSON replies.
    fn stub(server: &str, script: &str) -> StdioClient {
        StdioClient::spawn(server, "/bin/sh", &["-c".to_string(), script.to_string()])
            .expect("the stub server spawns")
    }

    #[test]
    fn request_encodes_exactly_one_json_line() {
        let line = request(7, "tools/list", json!({})).unwrap();
        // `serde_json`'s default map is key-ordered, so this encoding is
        // byte-stable across runs and platforms.
        assert_eq!(
            line,
            r#"{"id":7,"jsonrpc":"2.0","method":"tools/list","params":{}}"#
        );
        assert!(!line.contains('\n'));
        let back: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["jsonrpc"], json!("2.0"));
        assert_eq!(back["id"], json!(7));
        assert_eq!(back["method"], json!("tools/list"));
        assert_eq!(back["params"], json!({}));
        // A null params means "omitted" (JSON-RPC forbids a null params
        // member), so the member is absent rather than present-and-null.
        let line = request(1, "initialize", Value::Null).unwrap();
        assert_eq!(line, r#"{"id":1,"jsonrpc":"2.0","method":"initialize"}"#);
    }

    #[test]
    fn a_newline_inside_params_stays_escaped_on_one_line() {
        let line = request(2, "tools/call", json!({"arguments": {"text": "a\nb\n"}})).unwrap();
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(!line.contains('\n'), "{line}");
        // The escape is two characters in the line and one in the value.
        assert!(line.contains(r#""a\nb\n""#), "{line}");
        let back: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["params"]["arguments"]["text"], json!("a\nb\n"));
        assert_eq!(
            back["params"]["arguments"]["text"].as_str().unwrap().len(),
            4
        );

        // The same holds for the notification path and for a raw newline
        // smuggled in a method-name-shaped string.
        let line = notification("notifications/initialized", json!({"note": "x\r\ny"})).unwrap();
        assert_eq!(line.lines().count(), 1, "{line}");
        assert!(!line.contains('\r'), "{line}");
    }

    #[test]
    fn a_blank_method_or_non_container_params_is_refused() {
        for method in ["", "   ", "\t"] {
            let err = request(1, method, json!({})).unwrap_err();
            assert!(err.contains("method"), "{err}");
            assert!(notification(method, Value::Null).is_err());
        }
        let err = request(1, "tools/list", json!(5)).unwrap_err();
        assert!(err.contains("params") && err.contains('5'), "{err}");
    }

    #[test]
    fn a_notification_carries_no_id_and_keeps_its_method() {
        let line = notification("notifications/initialized", Value::Null).unwrap();
        assert_eq!(
            line,
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
        );
        let back: Value = serde_json::from_str(&line).unwrap();
        assert!(back.get("id").is_none(), "{back}");
        assert!(back.get("params").is_none(), "{back}");
    }

    #[test]
    fn decode_requires_jsonrpc_2_0_and_an_integer_id() {
        for bad in [
            r#"{"id":1,"result":{}}"#,
            r#"{"jsonrpc":"1.0","id":1,"result":{}}"#,
            r#"{"jsonrpc":2,"id":1,"result":{}}"#,
        ] {
            let err = decode(bad).unwrap_err();
            assert!(err.contains("jsonrpc"), "{bad} → {err}");
        }
        let err = decode(r#"{"jsonrpc":"2.0","id":"1","result":{}}"#).unwrap_err();
        assert!(err.contains("id"), "{err}");
        let err = decode(r#"{"jsonrpc":"2.0","result":{}}"#).unwrap_err();
        assert!(err.contains("id"), "{err}");
        let err = decode("[1,2]").unwrap_err();
        assert!(err.contains("object"), "{err}");
        let err = decode("not json at all").unwrap_err();
        assert!(err.contains("not JSON"), "{err}");
    }

    #[test]
    fn decode_requires_exactly_one_of_result_and_error() {
        let both =
            decode(r#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#)
                .unwrap_err();
        assert!(both.contains("both"), "{both}");
        let neither = decode(r#"{"jsonrpc":"2.0","id":1}"#).unwrap_err();
        assert!(neither.contains("neither"), "{neither}");
        // A `result` of null is a result, not a missing member.
        let null_result = decode(r#"{"jsonrpc":"2.0","id":1,"result":null}"#).unwrap();
        assert!(null_result.get("result").unwrap().is_null());
        let ok = decode(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#).unwrap();
        assert_eq!(ok["result"]["tools"], json!([]));
    }

    #[test]
    fn decode_response_rejects_a_mismatched_id_and_names_both_ids() {
        let line = r#"{"jsonrpc":"2.0","id":9,"result":{}}"#;
        let err = decode_response(line, 1).unwrap_err();
        assert!(err.contains('9') && err.contains('1'), "{err}");
        assert!(decode_response(line, 9).is_ok());
        // A negative id can never match a u64 request id.
        let err = decode_response(r#"{"jsonrpc":"2.0","id":-1,"result":{}}"#, 1).unwrap_err();
        assert!(err.contains("-1"), "{err}");
    }

    #[test]
    fn result_of_returns_the_result_and_surfaces_the_peers_error() {
        let msg = decode(r#"{"jsonrpc":"2.0","id":1,"result":{"tools":[]}}"#).unwrap();
        assert_eq!(result_of(&msg).unwrap(), &json!({"tools": []}));

        let msg = decode(
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"no such method"}}"#,
        )
        .unwrap();
        let err = result_of(&msg).unwrap_err();
        assert_eq!(err.code, -32601);
        assert_eq!(err.message, "no such method");
        assert!(err.to_string().contains("-32601"), "{err}");
    }

    #[test]
    fn result_of_flags_every_unreadable_envelope_as_invalid_request() {
        // Neither member, both members, a non-object, an error without a
        // message, and an error with a non-integer code.
        let cases = [
            (json!({"jsonrpc": "2.0", "id": 1}), "neither"),
            (json!({"result": {}, "error": {}}), "both"),
            (json!([1, 2]), "object"),
            (json!({"error": {"code": -1}}), "message"),
            (json!({"error": {"code": "boom", "message": "x"}}), "code"),
        ];
        for (msg, needle) in cases {
            let err = result_of(&msg).unwrap_err();
            assert_eq!(err.code, CODE_INVALID_REQUEST, "{msg} → {err}");
            assert!(err.message.contains(needle), "{msg} → {err}");
        }
        // A non-message error is still reported by its own code, with the
        // unreadable part named.
        let err = result_of(&json!({"error": {"code": -32000}})).unwrap_err();
        assert_eq!(err.code, CODE_INVALID_REQUEST);
        assert!(err.message.contains("-32000"), "{err}");
    }

    #[test]
    fn tool_specs_translates_two_tools_in_server_order_with_documented_defaults() {
        let fixture = json!({
            "tools": [
                {
                    "name": "read_file",
                    "description": "Read a file",
                    "inputSchema": {
                        "type": "object",
                        "properties": { "path": { "type": "string" } },
                        "required": ["path"]
                    }
                },
                { "name": "ping" }
            ]
        });
        let specs = tool_specs(&fixture).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "read_file");
        assert_eq!(specs[0].description, "Read a file");
        assert_eq!(
            specs[0].input_schema,
            json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]})
        );
        // Absent description and schema get the two documented defaults; the
        // schema default is the empty-object shape, i.e. "no arguments".
        assert_eq!(specs[1].name, "ping");
        assert_eq!(specs[1].description, DEFAULT_TOOL_DESCRIPTION);
        assert_eq!(
            specs[1].input_schema,
            json!({"type": "object", "properties": {}})
        );
        // An explicit null is the same as absent, and an empty list is empty.
        let specs = tool_specs(&json!({
            "tools": [{"name": "t", "description": null, "inputSchema": null}]
        }))
        .unwrap();
        assert_eq!(specs[0].description, DEFAULT_TOOL_DESCRIPTION);
        assert_eq!(specs[0].input_schema, empty_object_schema());
        assert!(tool_specs(&json!({"tools": []})).unwrap().is_empty());
    }

    #[test]
    fn tool_specs_names_the_offending_index_for_every_malformed_entry() {
        let cases = [
            (
                json!({"tools": [{"name": "ok"}, {"description": "no name"}]}),
                "#1",
            ),
            (json!({"tools": [{"name": "ok"}, {"name": ""}]}), "#1"),
            (json!({"tools": [{"name": "ok"}, "not-an-object"]}), "#1"),
            (
                json!({"tools": [{"name": "ok"}, {"name": "b", "description": 3}]}),
                "#1",
            ),
            (
                json!({"tools": [{"name": "ok"}, {"name": "b", "inputSchema": []}]}),
                "#1",
            ),
        ];
        for (fixture, needle) in cases {
            let err = tool_specs(&fixture).unwrap_err();
            assert!(err.contains(needle), "{fixture} → {err}");
        }
        // A malformed container is named too.
        assert!(tool_specs(&json!({})).unwrap_err().contains("no `tools`"));
        assert!(tool_specs(&json!({"tools": {}}))
            .unwrap_err()
            .contains("must be an array"));
    }

    #[test]
    fn namespaced_sanitizes_both_parts_and_never_yields_a_blank_part() {
        assert_eq!(namespaced("fs", "read_file"), "mcp__fs__read_file");
        assert_eq!(
            namespaced("my-server", "read-file"),
            "mcp__my_server__read_file"
        );
        assert_eq!(namespaced("a.b/c", "x y"), "mcp__a_b_c__x_y");
        // A blank part collapses to `_` rather than producing a part-less
        // name: `mcp__` + `_` + `__` + `_`.
        assert_eq!(namespaced("", ""), "mcp______");
        assert_eq!(namespaced("  ", "get"), "mcp______get");
        // A multi-byte character costs exactly one `_` and the result stays
        // ASCII (the cap never splits a char, because there is no cap here —
        // every char maps to one).
        let ns = namespaced("srv", "héllo→x");
        assert_eq!(ns, "mcp__srv__h_llo_x");
        assert!(ns.is_ascii());
    }

    #[test]
    fn collides_with_resident_catches_shadowing_and_leaves_other_names_alone() {
        // The bare name of a resident tool.
        assert!(collides_with_resident("fs", "bash"));
        // The same name with different case: the model cannot tell them apart.
        assert!(collides_with_resident("fs", "Bash"));
        // A sanitized bare name that lands on a resident name.
        assert!(collides_with_resident("fs", "repo map"));
        // The prefixed form of a resident name is a collision too, because the
        // prefix is dropped if the caller ever registers the bare form.
        assert!(collides_with_resident("fs", "read"));
        // Names that are merely similar, or that only exist behind the prefix,
        // are left alone: the guard must not block every MCP server outright.
        assert!(!collides_with_resident("x", "read_file"));
        assert!(!collides_with_resident("fs", "get-weather"));
        assert!(!collides_with_resident("fs", "grep-all"));
    }

    #[test]
    fn spawn_refuses_a_blank_server_id_or_program() {
        let err = StdioClient::spawn("", "/bin/sh", &[]).unwrap_err();
        assert!(err.contains("server id"), "{err}");
        let err = StdioClient::spawn("s", "  ", &[]).unwrap_err();
        assert!(err.contains("program"), "{err}");
        // A program that does not exist names the server and the program.
        let err = StdioClient::spawn("ghost", "/nonexistent/mcp-server", &[]).unwrap_err();
        assert!(
            err.contains("ghost") && err.contains("/nonexistent/mcp-server"),
            "{err}"
        );
    }

    #[test]
    fn initialize_handshakes_with_a_stub_server_over_newline_delimited_stdio() {
        // The stub answers the first line it reads (our `initialize` request,
        // which always carries id 1) and then drains the required
        // `notifications/initialized` line before exiting.
        let script = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stub","version":"1"}}}'
read initialized
exit 0"#;
        let mut client = stub("stub", script);
        assert_eq!(client.server(), "stub");
        let result = client
            .initialize("overseer", "0.1.0")
            .expect("the handshake succeeds");
        assert_eq!(result["serverInfo"]["name"], json!("stub"));
        assert_eq!(result["serverInfo"]["version"], json!("1"));
        assert_eq!(result["protocolVersion"], json!(PROTOCOL_VERSION));
        // The server exited on stdin EOF; shutdown reaps it without a kill.
        client.shutdown().expect("the stub exits once stdin closes");
    }

    #[test]
    fn list_tools_and_call_tool_run_the_scripted_handshake_over_stdio() {
        let script = r#"read init
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stub","version":"1"}}}'
read initialized
read list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"tools":[{"name":"echo"},{"name":"fail","inputSchema":{"type":"object","properties":{}}}]}}'
read call
printf '%s\n' '{"jsonrpc":"2.0","id":3,"result":{"content":[{"type":"text","text":"boom"}],"isError":true}}'
exit 0"#;
        let mut client = stub("stub", script);
        client.initialize("overseer", "0.1.0").unwrap();
        let specs = client
            .list_tools()
            .expect("tools/list decodes through the client");
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].name, "echo");
        assert_eq!(specs[0].description, DEFAULT_TOOL_DESCRIPTION);
        assert_eq!(specs[1].name, "fail");
        // A tool-level `isError` is a result, not a transport failure.
        let out = client
            .call_tool("fail", json!({"text": "hi"}))
            .expect("a tool-level error is still a result");
        assert_eq!(out["isError"], json!(true));
        assert_eq!(out["content"][0]["text"], json!("boom"));
        assert!(client.shutdown().is_ok());
    }

    #[test]
    fn a_stub_that_exits_before_answering_fails_loudly_without_hanging() {
        // Reads our request, exits without replying: stdout closes.
        let mut client = stub("dead-stub", "read line\nexit 0");
        let err = client.initialize("overseer", "0.1.0").unwrap_err();
        assert!(err.contains("dead-stub"), "{err}");
        assert!(err.contains("exited early"), "{err}");

        // Exits before even reading: the write or the read fails, either way
        // an error naming the server — and never a silent success.
        let mut client = stub("dead-at-once", "exit 0");
        let err = client.initialize("overseer", "0.1.0").unwrap_err();
        assert!(err.contains("dead-at-once"), "{err}");

        // A reply with the wrong id (a stale/desynchronized line) is refused.
        let script = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":42,"result":{}}'
read initialized
exit 0"#;
        let mut client = stub("wrong-id", script);
        let err = client.initialize("overseer", "0.1.0").unwrap_err();
        assert!(err.contains("wrong-id") && err.contains("42"), "{err}");

        // A reply from another revision is refused rather than talked to.
        let script = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"old","version":"1"}}}'
read initialized
exit 0"#;
        let mut client = stub("old", script);
        let err = client.initialize("overseer", "0.1.0").unwrap_err();
        assert!(
            err.contains("2024-11-05") && err.contains(PROTOCOL_VERSION),
            "{err}"
        );
    }

    #[test]
    fn shutdown_closes_stdin_reaps_the_child_and_refuses_a_second_shutdown() {
        // A server that answers the handshake and then ignores stdin EOF must
        // be killed, and told so.
        let script = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stubborn","version":"1"}}}'
read initialized
sleep 30"#;
        let mut client = stub("stubborn", script);
        client.initialize("overseer", "0.1.0").unwrap();
        let err = client.shutdown().unwrap_err();
        assert!(err.contains("stubborn") && err.contains("killed"), "{err}");
        let err = client.shutdown().unwrap_err();
        assert!(err.contains("already shut down"), "{err}");
        // Calls after shutdown teach instead of writing to a dead pipe.
        let err = client.call("tools/list", json!({})).unwrap_err();
        assert!(err.contains("stdin is already closed"), "{err}");
    }

    #[test]
    fn an_rpc_error_response_becomes_an_err_naming_the_server_and_code() {
        let script = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"stub","version":"1"}}}'
read initialized
read list
printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"no tools here"}}'
exit 0"#;
        let mut client = stub("stub", script);
        client.initialize("overseer", "0.1.0").unwrap();
        let err = client.list_tools().unwrap_err();
        assert!(
            err.contains("stub") && err.contains("-32601") && err.contains("no tools here"),
            "{err}"
        );
    }

    #[test]
    fn call_timeout_bounds_every_call_at_thirty_seconds() {
        assert_eq!(CALL_TIMEOUT, Duration::from_secs(30));
    }

    #[test]
    fn call_with_timeout_returns_a_fast_answer() {
        // Answers the request line at once, then exits.
        let script = r#"read line
printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}'
exit 0"#;
        let mut client = stub("fast", script);
        let msg = client
            .call_with_timeout("tools/list", json!({}), Duration::from_secs(5))
            .expect("a fast server answers inside its timeout");
        assert_eq!(result_of(&msg).unwrap(), &json!({"ok": true}));
        assert_eq!(client.next_id, 2);
        assert!(client.shutdown().is_ok());
    }

    #[test]
    fn call_with_timeout_kills_a_hung_server_and_burns_the_id() {
        // Reads our request and never answers: without the timeout this call
        // would block until the child exits on its own. `exec` replaces the
        // shell with `sleep`, so the kill lands on the sleeper itself and the
        // pipe closes at once instead of lingering on an orphaned child.
        let script = "read line\nexec sleep 30";
        let mut client = stub("hung", script);
        let err = client
            .call_with_timeout("tools/list", json!({}), Duration::from_millis(50))
            .unwrap_err();
        assert!(err.contains("hung"), "{err}");
        assert!(err.contains("tools/list"), "{err}");
        assert!(err.contains("(id 1)"), "{err}");
        assert!(err.contains("50"), "{err}");
        assert!(err.contains("timeout"), "{err}");
        assert!(err.contains("restart the server"), "{err}");
        // The id is burned: the next call would carry id 2.
        assert_eq!(client.next_id, 2);
        // The child is dead and reaped within the bounded grace.
        let child = client.child.as_mut().expect("reaped, not detached");
        assert!(child.try_wait().unwrap().is_some());
        // The session is dead: the next call fails naming the shutdown state
        // instead of writing to a dead pipe ...
        let err = client.call("tools/list", json!({})).unwrap_err();
        assert!(err.contains("stdin is already closed"), "{err}");
        // ... and it burns the next id too.
        assert_eq!(client.next_id, 3);
    }

    #[test]
    fn a_child_that_will_not_die_is_detached_within_the_grace_not_waited_on() {
        // Stand-in for a child stuck past SIGKILL (uninterruptible sleep,
        // which a unit test cannot produce): an unkilled sleeper. The
        // bounded reap must give up at the grace and hand the child off.
        let child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 5"])
            .spawn()
            .unwrap();
        let t = Instant::now();
        let reaped = reap_or_detach(child, Duration::from_millis(100));
        assert!(reaped.is_none(), "a live child cannot be reaped");
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());

        let mut done = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .spawn()
            .unwrap();
        let _ = done.wait();
        assert!(reap_or_detach(done, Duration::from_millis(100)).is_some());
    }

    #[test]
    fn search_tools_ranks_exact_above_substring_above_overlap() {
        let specs = vec![
            ToolSpec {
                name: "read_file".into(),
                description: "read a file from disk".into(),
                input_schema: json!({}),
            },
            ToolSpec {
                name: "grep_search".into(),
                description: "search across files".into(),
                input_schema: json!({}),
            },
            ToolSpec {
                name: "write_file".into(),
                description: "write content to disk".into(),
                input_schema: json!({}),
            },
        ];
        // Exact name match scores 1.0 and leads.
        let hits = search_tools(&specs, "grep_search", 10);
        assert_eq!(
            hits,
            vec![("grep_search".to_string(), 1.0)],
            "only the exact name matches `grep_search` as a whole"
        );
        // Substring match scores 0.7.
        let hits = search_tools(&specs, "grep", 10);
        assert_eq!(hits, vec![("grep_search".to_string(), 0.7)]);
        // Token overlap fraction: `disk content` fully covers write_file
        // (content + disk → 1.0) and half-covers read_file (disk only →
        // 0.5), so overlap outranks nothing here but orders the pair.
        let hits = search_tools(&specs, "disk content", 10);
        assert_eq!(
            hits,
            vec![
                ("write_file".to_string(), 1.0),
                ("read_file".to_string(), 0.5),
            ]
        );
        // Equal scores tie-break by name ascending: both carry `disk`.
        let hits = search_tools(&specs, "disk", 10);
        assert_eq!(
            hits,
            vec![
                ("read_file".to_string(), 1.0),
                ("write_file".to_string(), 1.0),
            ]
        );
        // Cap, empty query, zero top_k, and no-match all behave.
        assert_eq!(search_tools(&specs, "disk content", 1).len(), 1);
        assert!(search_tools(&specs, "   ", 10).is_empty());
        assert!(search_tools(&specs, "grep", 0).is_empty());
        assert!(search_tools(&specs, "zzz-no-such-tool", 10).is_empty());
    }

    #[test]
    fn trim_description_keeps_a_char_boundary_head_with_marker() {
        assert_eq!(trim_description("anything", 0), "");
        assert_eq!(trim_description("short", 5), "short");
        assert_eq!(trim_description("abcdef", 4), "abcd…");
        // Multi-byte chars count as one char each; the cut never splits one.
        assert_eq!(trim_description("héllo wörld", 5), "héllo…");
    }
}
