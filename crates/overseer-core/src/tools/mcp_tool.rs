//! The `mcp` resident tool — one op tool over the configured MCP servers.
//!
//! **Why one tool and not N.** MCP tool definitions are chosen by
//! third-party servers, so their names, count and descriptions are
//! discovered *at runtime*. Registering them in `ToolRegistry.specs` would
//! put volatile, session-dependent text into the advertised tool array —
//! exactly the cache-killing churn Invariant 2 forbids (and a server that
//! reorders its list mid-session would invalidate the prefix on every turn).
//! So the registry advertises exactly one spec, `mcp`, with a fixed schema,
//! and the discovered tools live behind it:
//!
//! - `{op:"search", query}` — find tools; `query` absent/empty lists every
//!   discovered tool name with a one-line description (capped). Ranked
//!   search is `mcp::search_tools` (exact name > prefix > substring > token
//!   overlap).
//! - `{op:"call", tool, args}` — run one by its full `mcp__<server>__<tool>`
//!   name. `args` is validated against that tool's own `inputSchema` with the
//!   registry's `check_args`, so a typo is refused before the server sees it.
//!
//! **Lazy, one-way lifetimes.** A server is spawned (env-allowlisted, see
//! `StdioClient::spawn_with_env`) plus `initialize` + `tools/list` on the
//! first op that needs it, and its tool list is cached for the session. A
//! call that fails because the server died or timed out removes it from the
//! map, so the next use respawns instead of talking to a corpse. Dropping the
//! state drops every client, which kills and reaps its child
//! (`StdioClient::Drop`) — no path leaks a server process.
//!
//! **Collisions are skipped, not renamed.** A discovered name that
//! `mcp::collides_with_resident` reports is dropped from the cached list, so
//! it can never be called: an ungated remote tool standing in for a
//! permission-gated resident one (`bash`, `edit`) would be a privilege
//! escalation (module header of `mcp.rs`).

use std::collections::HashMap;
use std::time::Duration;

use serde_json::{json, Value};

use super::{check_args, schema, ToolOutput};
use crate::mcp::{self, StdioClient};
use crate::mcp_config::{McpServer, Trust};
use crate::provider::ToolSpec;

/// Longest list `op=search` prints for an empty query (tool names + one-line
/// descriptions). Results for a real query are capped at [`TOP_K`].
const LIST_CAP: usize = 50;
/// Ranked hits returned for a non-empty query.
const TOP_K: usize = 5;
/// Char cap on a description in the output (the full description can be
/// arbitrarily long; the model asks the server for nothing more by reading a
/// truncated line).
pub(crate) const DESC_CAP: usize = 200;
/// Tool names named in an unknown-tool error.
const UNKNOWN_TOOL_CAP: usize = 20;

/// The one resident spec. Fixed text and a fixed schema — the whole point of
/// the design (see the module header): this array is byte-stable whether the
/// session has one MCP server or twenty.
pub fn spec() -> ToolSpec {
    ToolSpec {
        name: "mcp".to_string(),
        description: "Use tools from configured MCP servers. op=search {query} finds tools \
                      (name, description, input schema); op=call {tool, args} runs one by its \
                      full name from search."
            .to_string(),
        input_schema: schema(
            json!({
                "op": {
                    "type": "string",
                    "description": "search: find tools by query | call: run one tool"
                },
                "query": {
                    "type": "string",
                    "description": "search terms (op=search; empty lists all discovered tools)"
                },
                "tool": {
                    "type": "string",
                    "description": "full mcp__<server>__<tool> name from op=search (op=call)"
                },
                "args": {
                    "type": "object",
                    "description": "arguments object for that tool's input schema (op=call)"
                }
            }),
            &["op"],
        ),
    }
}

/// The sanitized server ids the policy may treat as read-only: the servers
/// the operator declared `trust: "read"` (see `perm::Policy::mcp_read_servers`).
pub fn read_server_ids(servers: &[McpServer]) -> Vec<String> {
    servers
        .iter()
        .filter(|s| s.trust == Trust::Read)
        .map(|s| mcp::sanitized(&s.name))
        .collect()
}

/// Configured servers + the live ones this session has started.
pub struct McpState {
    /// Every configured server, sorted by name (`mcp_config::load`).
    servers: Vec<McpServer>,
    /// Spawned servers by id, each with its cached, collision-filtered tool
    /// list. Absent = not started yet (or dropped after a failed call).
    live: HashMap<String, Live>,
    /// Bound on one `tools/call` — [`mcp::CALL_TIMEOUT`] unless a caller
    /// narrows it (the hung-server test drives the timeout path this way).
    call_timeout: Duration,
}

/// One live server: the client, the tools it advertised (namespaced, in the
/// server's own order, minus collisions), and the names skipped as shadows.
struct Live {
    client: StdioClient,
    tools: Vec<McpTool>,
    /// Namespaced names dropped because they shadow a resident tool. Kept so
    /// a call to one can say *why* it is not callable instead of pretending
    /// the server never offered it.
    shadowed: Vec<String>,
}

/// One discovered tool: the server's own name (what `tools/call` must send)
/// plus the namespaced spec the model sees.
struct McpTool {
    bare: String,
    spec: ToolSpec,
}

impl McpState {
    /// Build the state for `servers` (empty = the caller should not have
    /// added the spec at all; see `ToolRegistry::with_mcp`).
    pub fn new(servers: Vec<McpServer>) -> Self {
        McpState {
            servers,
            live: HashMap::new(),
            call_timeout: mcp::CALL_TIMEOUT,
        }
    }

    /// Dispatch one `mcp` op. Errors are `ToolOutput`s that teach the
    /// contract (never a panic, never a silent no-op).
    pub fn run(&mut self, input: &Value) -> ToolOutput {
        let op = input
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        match op.as_str() {
            "search" => self.search(input.get("query").and_then(Value::as_str)),
            "call" => {
                let Some(tool) = input.get("tool").and_then(Value::as_str) else {
                    return ToolOutput::err(
                        "mcp op=call needs `tool` — pass a full name from op=search.",
                    );
                };
                let args = input.get("args").cloned().unwrap_or_else(|| json!({}));
                self.call(tool, &args)
            }
            "" => ToolOutput::err("mcp needs `op` — search {query} or call {tool, args}."),
            other => ToolOutput::err(format!("mcp: unknown op '{other}' — use search or call.")),
        }
    }

    /// `op=search`: start every configured server that isn't live yet
    /// (per-server spawn errors become lines in the output — one broken
    /// server must not hide the others' tools), then list or rank.
    fn search(&mut self, query: Option<&str>) -> ToolOutput {
        if self.servers.is_empty() {
            return ToolOutput::ok("mcp: no MCP servers configured.".to_string());
        }
        let notes = self.discover();
        let query = query.unwrap_or("").trim();
        let mut out = if query.is_empty() {
            self.list_all()
        } else {
            self.search_ranked(query)
        };
        for note in notes {
            out.push('\n');
            out.push_str(&note);
        }
        ToolOutput::ok(out)
    }

    /// Start every configured server that isn't live yet. Returns one note
    /// per spawn failure or shadowed-name skip, for the search output.
    pub(crate) fn discover(&mut self) -> Vec<String> {
        let mut notes: Vec<String> = Vec::new();
        let servers = self.servers.clone();
        for server in &servers {
            if self.live.contains_key(&server.name) {
                continue;
            }
            match Self::launch(server) {
                Ok(live) => {
                    if !live.shadowed.is_empty() {
                        notes.push(format!(
                            "! mcp server `{}`: {} tool(s) skipped — their names shadow resident tools ({})",
                            server.name,
                            live.shadowed.len(),
                            live.shadowed.join(", ")
                        ));
                    }
                    self.live.insert(server.name.clone(), live);
                }
                Err(e) => notes.push(format!("! {e}")),
            }
        }
        notes
    }

    /// Every discovered (callable) tool spec, in configured-server order.
    pub(crate) fn discovered(&self) -> Vec<ToolSpec> {
        self.servers
            .iter()
            .filter_map(|server| self.live.get(&server.name))
            .flat_map(|live| live.tools.iter())
            .map(|tool| tool.spec.clone())
            .collect()
    }

    /// The discovered input schema of a full `mcp__<server>__<tool>` name,
    /// starting its server if needed. `None` when no such tool exists.
    pub(crate) fn schema_of(&mut self, full: &str) -> Option<Value> {
        let server = self.server_for(full)?;
        self.ensure_live(&server).ok()?;
        self.live
            .get(&server)?
            .tools
            .iter()
            .find(|t| t.spec.name == full)
            .map(|t| t.spec.input_schema.clone())
    }

    /// Empty query: every discovered (callable) tool, name + one-line
    /// description, capped at [`LIST_CAP`].
    fn list_all(&self) -> String {
        let mut listed: Vec<(String, String)> = Vec::new();
        for server in &self.servers {
            if let Some(live) = self.live.get(&server.name) {
                for tool in &live.tools {
                    listed.push((tool.spec.name.clone(), tool.spec.description.clone()));
                }
            }
        }
        listed.sort();
        let total = listed.len();
        let shown = total.min(LIST_CAP);
        let mut out = format!("mcp: {total} tool(s) from {} server(s)", self.live.len());
        for (name, description) in &listed[..shown] {
            out.push('\n');
            out.push_str(&format!(
                "{name} — {}",
                one_line(&mcp::trim_description(description, DESC_CAP))
            ));
        }
        if total > shown {
            out.push_str(&format!("\n… and {} more", total - shown));
        }
        out
    }

    /// Non-empty query: `mcp::search_tools` over every live spec, [`TOP_K`]
    /// hits, each with its namespaced name, trimmed description and compact
    /// input schema.
    fn search_ranked(&self, query: &str) -> String {
        let specs: Vec<ToolSpec> = self
            .live
            .values()
            .flat_map(|live| live.tools.iter())
            .map(|tool| tool.spec.clone())
            .collect();
        let hits = mcp::search_tools(&specs, query, TOP_K);
        let mut out = format!(
            "mcp search {query:?}: {} hit(s) of {} tool(s)",
            hits.len(),
            specs.len()
        );
        for (name, score) in &hits {
            let Some(spec) = specs.iter().find(|s| &s.name == name) else {
                continue;
            };
            let schema_json =
                serde_json::to_string(&spec.input_schema).unwrap_or_else(|_| "{}".to_string());
            out.push('\n');
            out.push_str(&format!(
                "{name} ({score:.2}) — {}\n  schema: {schema_json}",
                one_line(&mcp::trim_description(&spec.description, DESC_CAP))
            ));
        }
        if hits.is_empty() {
            out.push_str("\nno matching tool — use an empty query to list every tool");
        }
        out
    }

    /// `op=call`: resolve the full name to a configured server, start it if
    /// needed, validate `args` against the tool's own schema, run it, map the
    /// result.
    fn call(&mut self, tool: &str, args: &Value) -> ToolOutput {
        let Some(server) = self.server_for(tool) else {
            return ToolOutput::err(format!(
                "mcp: `{tool}` is not a configured MCP tool — call names look like \
                 `mcp__<server>__<tool>`. Configured server(s): {}",
                self.server_names()
            ));
        };
        if let Err(e) = self.ensure_live(&server) {
            return ToolOutput::err(e);
        }
        // Resolve the tool first (immutable), then call (mutable): the
        // borrow checker is happier, and `check_args` runs before any
        // request leaves this process.
        let (bare, schema_value) = {
            let live = self
                .live
                .get(&server)
                .expect("ensure_live inserted the server");
            if live.shadowed.iter().any(|name| name == tool) {
                return ToolOutput::err(format!(
                    "mcp: `{tool}` is shadowed by a resident tool — a colliding name is skipped \
                     and can never be called."
                ));
            }
            let Some(entry) = live.tools.iter().find(|t| t.spec.name == tool) else {
                let total = live.tools.len();
                let names: Vec<&str> = live
                    .tools
                    .iter()
                    .take(UNKNOWN_TOOL_CAP)
                    .map(|t| t.spec.name.as_str())
                    .collect();
                return ToolOutput::err(format!(
                    "mcp: server `{server}` has no tool `{tool}`. Its tool(s) ({total}): {}{}",
                    names.join(", "),
                    if total > names.len() { ", …" } else { "" }
                ));
            };
            (entry.bare.clone(), entry.spec.input_schema.clone())
        };
        if let Err(e) = check_args(&schema_value, args) {
            return ToolOutput::err(format!(
                "mcp: arguments for `{tool}` were rejected — {}",
                e.text
            ));
        }
        let timeout = self.call_timeout;
        let result = {
            let live = self
                .live
                .get_mut(&server)
                .expect("ensure_live inserted the server");
            live.client
                .call_tool_with_timeout(&bare, args.clone(), timeout)
        };
        match result {
            Ok(value) => map_result(&value),
            Err(e) => {
                // Dead, hung or desynchronized: drop the client (its Drop
                // kills and reaps the child) so the next use respawns.
                self.live.remove(&server);
                ToolOutput::err(format!(
                    "{e} — server `{server}` dropped; the next use respawns it"
                ))
            }
        }
    }

    /// Which configured server a full `mcp__<server>__<tool>` name belongs
    /// to: the longest sanitized id whose prefix matches. Config load refuses
    /// ids where one sanitized name extends another on a namespace boundary,
    /// so "longest" is unambiguous.
    fn server_for(&self, full: &str) -> Option<String> {
        if !full.starts_with("mcp__") {
            return None;
        }
        self.servers
            .iter()
            .filter(|server| full.starts_with(&format!("mcp__{}__", mcp::sanitized(&server.name))))
            .max_by_key(|server| mcp::sanitized(&server.name).len())
            .map(|server| server.name.clone())
    }

    /// Start `name` if it is not live, caching its tool list.
    fn ensure_live(&mut self, name: &str) -> Result<(), String> {
        if self.live.contains_key(name) {
            return Ok(());
        }
        let Some(server) = self.servers.iter().find(|s| s.name == name).cloned() else {
            return Err(format!("mcp: no configured server named `{name}`"));
        };
        let live = Self::launch(&server)?;
        self.live.insert(server.name, live);
        Ok(())
    }

    /// Spawn + handshake + `tools/list`, dropping every colliding tool. A
    /// failure anywhere (spawn, handshake, list) leaves nothing behind — the
    /// client is dropped, which kills the child.
    fn launch(server: &McpServer) -> Result<Live, String> {
        let env: Vec<(String, String)> = server
            .env
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let mut client =
            StdioClient::spawn_with_env(&server.name, &server.command, &server.args, &env)?;
        client.initialize("overseer", env!("CARGO_PKG_VERSION"))?;
        let discovered = client.list_tools()?;
        let mut tools = Vec::with_capacity(discovered.len());
        let mut shadowed = Vec::new();
        for spec in discovered {
            let namespaced = mcp::namespaced(&server.name, &spec.name);
            if mcp::collides_with_resident(&server.name, &spec.name) {
                shadowed.push(namespaced);
                continue;
            }
            tools.push(McpTool {
                bare: spec.name,
                spec: ToolSpec {
                    name: namespaced,
                    description: spec.description,
                    input_schema: spec.input_schema,
                },
            });
        }
        Ok(Live {
            client,
            tools,
            shadowed,
        })
    }

    /// Configured server ids, for an error message.
    fn server_names(&self) -> String {
        let names: Vec<&str> = self.servers.iter().map(|s| s.name.as_str()).collect();
        if names.is_empty() {
            "(none)".to_string()
        } else {
            names.join(", ")
        }
    }
}

/// Map an MCP `tools/call` result to a `ToolOutput`: `content[].text` parts
/// joined with `\n`, any other part type noted as `[<type> content omitted]`,
/// and MCP's own tool-level `isError: true` marking the result an error (it
/// is a result, not a transport failure — see `mcp::StdioClient::call_tool`).
/// A result with no `content` parts at all (e.g. `structuredContent` only)
/// returns the raw result JSON rather than an empty string.
fn map_result(result: &Value) -> ToolOutput {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts: Vec<String> = Vec::new();
    if let Some(items) = result.get("content").and_then(Value::as_array) {
        for item in items {
            match item.get("type").and_then(Value::as_str) {
                Some("text") => {
                    if let Some(text) = item.get("text").and_then(Value::as_str) {
                        parts.push(text.to_string());
                    }
                }
                Some(other) => parts.push(format!("[{other} content omitted]")),
                None => parts.push("[unknown content omitted]".to_string()),
            }
        }
    }
    let text = if parts.is_empty() {
        serde_json::to_string(result).unwrap_or_else(|_| "{}".to_string())
    } else {
        parts.join("\n")
    };
    if is_error {
        ToolOutput::err(text)
    } else {
        ToolOutput::ok(text)
    }
}

/// Collapse whitespace runs so a multi-line description stays one line.
fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::Path;

    use crate::tools::{ToolCtx, ToolRegistry};

    /// A stub MCP server under `/bin/sh` (the pattern `mcp.rs`'s own tests
    /// use): it reads our lines and prints one-line JSON replies.
    fn stub(name: &str, script: &str) -> McpServer {
        McpServer {
            name: name.to_string(),
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: BTreeMap::new(),
            trust: Trust::Ask,
        }
    }

    /// A stub that answers the handshake, lists the tools in `tools_json`
    /// (a single-line JSON array literal), then answers one `tools/call`
    /// with `call_result_json` (a single-line JSON object literal).
    fn scripted(tools_json: &str, call_result_json: &str) -> String {
        let init = "{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\"2025-06-18\",\
             \"capabilities\":{},\"serverInfo\":{\"name\":\"stub\",\"version\":\"1\"}}}";
        let list =
            format!("{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":{tools_json}}}}}");
        let call = format!("{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{call_result_json}}}");
        format!(
            "read init\n\
             printf '%s\\n' '{init}'\n\
             read initialized\n\
             read list\n\
             printf '%s\\n' '{list}'\n\
             read call\n\
             printf '%s\\n' '{call}'\n\
             exit 0"
        )
    }

    fn ctx(dir: &Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagents: Default::default(),
            checkpoint: None,
            sandbox: false,
            broker: None,
        }
    }

    fn tmpdir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("overseer-mcp-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Spec list as JSON — the cache-safety comparison surface.
    fn specs_json(specs: &[ToolSpec]) -> String {
        let values: Vec<Value> = specs
            .iter()
            .map(|s| {
                json!({
                    "name": s.name,
                    "description": s.description,
                    "input_schema": s.input_schema,
                })
            })
            .collect();
        serde_json::to_string(&values).unwrap()
    }

    #[test]
    fn without_servers_the_registry_is_byte_identical_to_core() {
        // R1: zero configured servers must not change the advertised array.
        let plain = ToolRegistry::core(crate::perm::Policy::allow_all());
        let empty = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(Vec::new());
        assert_eq!(specs_json(&plain.specs), specs_json(&empty.specs));
        assert_eq!(plain.specs.len(), empty.specs.len());
        assert!(!empty.specs.iter().any(|s| s.name == "mcp"));
        // The `mcp` name is still a valid --no-tools value (TOOL_NAMES).
        assert!(crate::tools::TOOL_NAMES.contains(&"mcp"));
    }

    #[test]
    fn mcp_is_deferred_and_the_advertised_array_is_cache_stable_across_ops() {
        let dir = tmpdir();
        let server = stub(
            "stub",
            &scripted(
                r#"[{"name":"echo","description":"Echo back the text","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}},{"name":"add","description":"Add two numbers"}]"#,
                r#"{"content":[{"type":"text","text":"hello from echo"}]}"#,
            ),
        );
        let mut reg = ToolRegistry::core_with(
            crate::perm::Policy::allow_all(),
            crate::tools::Optional::ALL,
        )
        .with_mcp(vec![server]);
        let mut c = ctx(&dir);

        // The internal `mcp` op spec is resident but never advertised; the
        // only advertised change is `tools` naming MCP servers.
        let plain = ToolRegistry::core_with(
            crate::perm::Policy::allow_all(),
            crate::tools::Optional::ALL,
        );
        assert_eq!(reg.specs.len(), plain.specs.len());
        assert!(!reg.specs.iter().any(|s| s.name == "mcp"));
        let tools_desc = |r: &ToolRegistry| {
            r.specs
                .iter()
                .find(|s| s.name == "tools")
                .unwrap()
                .description
                .clone()
        };
        assert!(tools_desc(&reg).contains("MCP servers"));
        assert!(!tools_desc(&plain).contains("MCP"));
        let mcp_spec = reg.base_specs.iter().find(|s| s.name == "mcp").unwrap();
        assert_eq!(
            mcp_spec.input_schema.get("additionalProperties"),
            Some(&Value::Bool(false))
        );
        assert_eq!(mcp_spec.input_schema["required"], json!(["op"]));
        assert!(mcp_spec.description.contains("op=search"));
        let before = specs_json(&reg.specs);

        // Empty query lists both discovered tools, namespaced.
        let listed = reg.call("mcp", &json!({"op": "search"}), &mut c);
        assert!(!listed.is_error, "{}", listed.text);
        assert!(listed.text.contains("mcp__stub__echo"), "{}", listed.text);
        assert!(listed.text.contains("mcp__stub__add"), "{}", listed.text);

        // A query returns the matching name with its compact schema.
        let found = reg.call("mcp", &json!({"op": "search", "query": "echo"}), &mut c);
        assert!(found.text.contains("mcp__stub__echo"), "{}", found.text);
        assert!(
            !found.text.contains("mcp__stub__add"),
            "the query must filter: {}",
            found.text
        );
        assert!(found.text.contains("schema"), "{}", found.text);

        // A call runs the ORIGINAL (un-namespaced) tool name and returns the
        // joined text content.
        let called = reg.call(
            "mcp",
            &json!({"op": "call", "tool": "mcp__stub__echo", "args": {"text": "hi"}}),
            &mut c,
        );
        assert!(!called.is_error, "{}", called.text);
        assert_eq!(called.text, "hello from echo");

        // ... and the advertised array never moved: the whole point.
        assert_eq!(before, specs_json(&reg.specs));
        let via_tools = reg.call("tools", &json!({"op": "search", "query": "echo"}), &mut c);
        assert!(via_tools.text.contains("mcp__stub__echo"));
        assert_eq!(before, specs_json(&reg.specs));
    }

    #[test]
    fn mcp_tools_are_found_and_called_through_tools_and_latch_untrusted() {
        let dir = tmpdir();
        let server = stub(
            "stub",
            &scripted(
                r#"[{"name":"echo","description":"Echo text back. Second sentence.","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}}]"#,
                r#"{"content":[{"type":"text","text":"hello from echo"}]}"#,
            ),
        );
        let mut reg = ToolRegistry::core_with(
            crate::perm::Policy::allow_all(),
            crate::tools::Optional::ALL,
        )
        .with_mcp(vec![server]);
        let mut c = ctx(&dir);
        let found = reg.call("tools", &json!({"op": "search", "query": "echo"}), &mut c);
        assert!(!found.is_error, "{}", found.text);
        let hit: Value = serde_json::from_str(found.text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(hit["name"], "mcp__stub__echo");
        assert_eq!(hit["input_schema"]["required"], json!(["text"]));
        assert!(
            reg.taint_notices.iter().any(|n| n.contains("via mcp")),
            "server-authored descriptions latch untrusted: {:?}",
            reg.taint_notices
        );
        let no_args = reg.call(
            "tools",
            &json!({"op": "call", "name": "mcp__stub__echo"}),
            &mut c,
        );
        assert!(
            no_args.is_error && no_args.text.contains("\"required\""),
            "{}",
            no_args.text
        );
        let called = reg.call(
            "tools",
            &json!({"op": "call", "name": "mcp__stub__echo", "args": {"text": "hi"}}),
            &mut c,
        );
        assert!(!called.is_error, "{}", called.text);
        assert_eq!(called.text, "hello from echo");
    }

    #[test]
    fn a_resumed_session_replaying_old_mcp_calls_still_dispatches() {
        // Pre-`tools` sessions called the resident `mcp` op tool by name;
        // it is no longer advertised but the internal dispatch arm stays.
        let dir = tmpdir();
        let server = stub(
            "stub",
            &scripted(
                r#"[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}}}}]"#,
                r#"{"content":[{"type":"text","text":"hello from echo"}]}"#,
            ),
        );
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(vec![server]);
        assert!(!reg.specs.iter().any(|s| s.name == "mcp"));
        let mut c = ctx(&dir);
        let old = reg.call(
            "mcp",
            &json!({"op": "call", "tool": "mcp__stub__echo", "args": {"text": "hi"}}),
            &mut c,
        );
        assert!(!old.is_error, "{}", old.text);
        assert_eq!(old.text, "hello from echo");
    }

    #[test]
    fn no_tools_mcp_hides_and_refuses_mcp_through_tools() {
        let dir = tmpdir();
        let server = stub("stub", "exit 1");
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(vec![server]);
        reg.disable(&["mcp".to_string()]);
        let tools = reg.specs.iter().find(|s| s.name == "tools").unwrap();
        assert!(!tools.description.contains("MCP"), "{}", tools.description);
        let mut c = ctx(&dir);
        let search = reg.call("tools", &json!({"op": "search", "query": "stub"}), &mut c);
        assert!(!search.text.contains("mcp__"), "{}", search.text);
        assert!(reg.taint_notices.is_empty(), "no server was spawned");
        for input in [
            json!({"op": "call", "name": "mcp__stub__echo", "args": {}}),
            json!({"op": "call", "name": "mcp__stub__echo"}),
        ] {
            let out = reg.call("tools", &input, &mut c);
            assert!(
                out.is_error && out.text.contains("disabled"),
                "{}",
                out.text
            );
        }
    }

    #[test]
    fn arg_validation_rejects_a_bad_call_before_the_server_sees_it() {
        let dir = tmpdir();
        let server = stub(
            "stub",
            &scripted(
                r#"[{"name":"echo","description":"Echo","inputSchema":{"type":"object","properties":{"text":{"type":"string"}},"required":["text"],"additionalProperties":false}}]"#,
                r#"{"content":[{"type":"text","text":"unreachable"}]}"#,
            ),
        );
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(vec![server]);
        let mut c = ctx(&dir);
        // Missing required arg, and an unknown arg: both refused locally.
        for args in [json!({}), json!({"text": "hi", "bogus": 1})] {
            let out = reg.call(
                "mcp",
                &json!({"op": "call", "tool": "mcp__stub__echo", "args": args}),
                &mut c,
            );
            assert!(out.is_error, "{args} → {}", out.text);
            assert!(out.text.contains("rejected"), "{}", out.text);
        }
        // The envelope itself is strict too (B1-2: additionalProperties).
        let bad = reg.call("mcp", &json!({"op": "search", "bogus": 1}), &mut c);
        assert!(
            bad.is_error && bad.text.contains("Unknown parameter"),
            "{}",
            bad.text
        );
        // Unknown op / missing op / unknown tool are taught, not crashed.
        assert!(
            reg.call("mcp", &json!({"op": "frobnicate"}), &mut c)
                .is_error
        );
        assert!(reg
            .call("mcp", &json!({"op": "call", "tool": "bash"}), &mut c)
            .text
            .contains("not a configured MCP tool"));
    }

    #[test]
    fn a_colliding_tool_name_is_skipped_and_never_callable() {
        let dir = tmpdir();
        // `bash` is a resident tool: the guard must skip it (bare-name
        // collision), and the server's other tool stays callable.
        let server = stub(
            "fs",
            &scripted(
                r#"[{"name":"bash","description":"run a shell"},{"name":"safe","description":"ok"}]"#,
                r#"{"content":[{"type":"text","text":"never"}]}"#,
            ),
        );
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(vec![server]);
        let mut c = ctx(&dir);
        let listed = reg.call("mcp", &json!({"op": "search"}), &mut c);
        assert!(listed.text.contains("mcp__fs__safe"), "{}", listed.text);
        assert!(
            !listed.text.contains("mcp__fs__bash —"),
            "a shadowing name is not advertised as a tool: {}",
            listed.text
        );
        assert!(
            listed.text.contains("skipped"),
            "the skip is reported: {}",
            listed.text
        );
        let out = reg.call(
            "mcp",
            &json!({"op": "call", "tool": "mcp__fs__bash", "args": {}}),
            &mut c,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("shadowed"), "{}", out.text);
    }

    #[test]
    fn the_child_environment_is_an_allowlist() {
        let dir = tmpdir();
        // A parent-only variable with a unique name (tests run in parallel,
        // and the child must NOT see it) plus one the config declares.
        let parent_var = format!("OVERSEER_MCP_PARENT_{}", uuid::Uuid::now_v7().simple());
        std::env::set_var(&parent_var, "leaked-from-parent");
        let script = format!(
            "read init\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{{\"protocolVersion\":\
             \"2025-06-18\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"envstub\",\"version\":\"1\"}}}}}}'\n\
             read initialized\n\
             read list\n\
             printf '%s\\n' '{{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{{\"tools\":[{{\"name\":\"showenv\",\
             \"description\":\"show env\",\"inputSchema\":{{\"type\":\"object\",\"properties\":{{}}}}}}]}}}}'\n\
             read call\n\
             printf '{{\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{{\"content\":[{{\"type\":\"text\",\
             \"text\":\"declared=%s parent=[%s]\"}}]}}}}\\n' \"$DECLARED\" \"${{{parent_var}}}\"\n\
             exit 0"
        );
        let mut server = stub("envstub", &script);
        server
            .env
            .insert("DECLARED".to_string(), "present-value".to_string());
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(vec![server]);
        let mut c = ctx(&dir);
        let out = reg.call(
            "mcp",
            &json!({"op": "call", "tool": "mcp__envstub__showenv", "args": {}}),
            &mut c,
        );
        assert!(!out.is_error, "{}", out.text);
        assert!(
            out.text.contains("declared=present-value"),
            "a declared env pair reaches the child: {}",
            out.text
        );
        assert!(
            out.text.contains("parent=[]"),
            "a parent-only variable must not: {}",
            out.text
        );
        assert!(!out.text.contains("leaked-from-parent"), "{}", out.text);
    }

    #[test]
    fn an_ablated_mcp_spec_is_absent_and_refused_without_spawning() {
        // `--no-tools mcp` must remove the spec AND refuse dispatch (the
        // same two-step every other ablation gets) — and must not spawn a
        // server on the way to saying no.
        let dir = tmpdir();
        let server = stub(
            "stub",
            &scripted(
                r#"[{"name":"ping","description":"pong"}]"#,
                r#"{"content":[]}"#,
            ),
        );
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(vec![server]);
        reg.disable(&["mcp".to_string()]);
        assert!(!reg.specs.iter().any(|s| s.name == "mcp"));
        let mut c = ctx(&dir);
        let out = reg.call("mcp", &json!({"op": "search"}), &mut c);
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("disabled"), "{}", out.text);
        assert!(
            reg.mcp.as_ref().unwrap().live.is_empty(),
            "a refused call must not have started a server"
        );
    }

    #[test]
    fn a_hung_server_fails_within_the_timeout_and_is_dropped() {
        let dir = tmpdir();
        let script = "read init\n\
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"protocolVersion\":\
             \"2025-06-18\",\"capabilities\":{},\"serverInfo\":{\"name\":\"hung\",\"version\":\"1\"}}}'\n\
             read initialized\n\
             read list\n\
             printf '%s\\n' '{\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"tools\":[{\"name\":\"hang\",\
             \"description\":\"never answers\"}]}}'\n\
             read call\n\
             exec sleep 30";
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all())
            .with_mcp(vec![stub("hung", script)]);
        reg.mcp
            .as_mut()
            .expect("with_mcp installs the state")
            .call_timeout = Duration::from_millis(250);
        let mut c = ctx(&dir);
        let out = reg.call(
            "mcp",
            &json!({"op": "call", "tool": "mcp__hung__hang", "args": {}}),
            &mut c,
        );
        assert!(out.is_error, "{}", out.text);
        assert!(out.text.contains("timeout"), "{}", out.text);
        assert!(out.text.contains("hung"), "{}", out.text);
        // The corpse is dropped: the next use respawns instead of reusing it.
        let state = reg.mcp.as_ref().unwrap();
        assert!(
            !state.live.contains_key("hung"),
            "a timed-out server must leave the live map"
        );
    }

    #[test]
    fn read_trust_ids_are_sanitized_server_names() {
        let mut a = stub("my-server", "");
        a.trust = Trust::Read;
        let b = stub("other", "");
        let ids = read_server_ids(&[a, b]);
        assert_eq!(ids, vec!["my_server".to_string()]);
    }

    #[test]
    fn mapping_joins_text_parts_notes_others_and_honors_is_error() {
        let ok = map_result(&json!({
            "content": [
                {"type": "text", "text": "one"},
                {"type": "image", "data": "…"},
                {"type": "text", "text": "two"}
            ]
        }));
        assert!(!ok.is_error);
        assert_eq!(ok.text, "one\n[image content omitted]\ntwo");
        let failed = map_result(&json!({
            "content": [{"type": "text", "text": "boom"}],
            "isError": true
        }));
        assert!(failed.is_error);
        assert_eq!(failed.text, "boom");
        // No content parts at all → the raw result, never an empty string.
        let bare = map_result(&json!({"structuredContent": {"a": 1}}));
        assert!(bare.text.contains("structuredContent"), "{}", bare.text);
    }

    #[test]
    fn first_use_spawns_lazily_and_building_the_registry_spawns_nothing() {
        let dir = tmpdir();
        let server = stub(
            "lazy",
            &scripted(
                r#"[{"name":"ping","description":"pong"}]"#,
                r#"{"content":[{"type":"text","text":"pong"}]}"#,
            ),
        );
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all()).with_mcp(vec![server]);
        let mut c = ctx(&dir);
        assert!(
            reg.mcp.as_ref().unwrap().live.is_empty(),
            "building the registry must not spawn anything"
        );
        let out = reg.call("mcp", &json!({"op": "search"}), &mut c);
        assert!(out.text.contains("mcp__lazy__ping"), "{}", out.text);
        assert_eq!(reg.mcp.as_ref().unwrap().live.len(), 1);
    }
}
