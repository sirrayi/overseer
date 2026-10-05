//! MCP server configuration — which stdio servers the `mcp` tool may drive.
//!
//! Global, one file: `~/.overseer/mcp.json` (see [`default_path`]). The shape
//! is the one the official clients use, so an operator can copy a
//! `mcpServers` block between tools:
//!
//! ```json
//! { "mcpServers": {
//!     "fs":    { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-filesystem", "/ws"] },
//!     "docs":  { "command": "mcp-docs", "args": [], "env": { "TOKEN": "${DOCS_TOKEN}" }, "trust": "read" }
//! } }
//! ```
//!
//! Two rules make this file a trust boundary rather than a convenience:
//!
//! 1. **`${VAR}` in an env value** is expanded from the parent process
//!    environment at load time, so a secret never has to be written into the
//!    file. An unset referenced variable is a load error naming the variable —
//!    a server silently missing its credential fails later and confusingly.
//! 2. **`trust`** is `"ask"` (default) or `"read"`. `read` states that the
//!    operator has decided this server only reads, so `mcp` calls to it skip
//!    the approval ladder (see `perm::Policy::mcp_read_servers`). The name is
//!    the only thing consulted at that decision point; it is a declaration by
//!    the operator, not a discovery about the server.
//!
//! The child environment is *not* the parent's: the registry spawns every
//! server through `StdioClient::spawn_with_env` (PATH/HOME + declared pairs
//! only). This file is where those pairs are declared.
//!
//! `// DEFERRED(owner): project-level `.overseer/mcp.json` — a cloned repo could spawn arbitrary programs; needs a trust prompt first — gate: trust-prompt UX`
//!
//! Parsing is strict and names the server and the field on every failure: a
//! typo in a config that starts processes must be a one-line diagnosis.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::Value;

/// How much the operator trusts a configured server at the permission gate.
///
/// This is a declaration, not a capability: a server claiming `read` can
/// still do anything its program does. The point is that the *operator*
/// decided, in a file, before the session started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Trust {
    /// Every `call` needs the normal gate (WorkspaceWrite: external-comms
    /// ladder → Ask by default). The default: unknown servers are not trusted.
    #[default]
    Ask,
    /// `call`s to this server are reads — allowed without a human.
    Read,
}

/// One configured MCP server: the program to spawn, its arguments, the env
/// pairs its child gets on top of PATH/HOME, and the operator's trust.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServer {
    /// Server id — namespaces its tools (`mcp__<name>__<tool>`).
    pub name: String,
    /// Program to spawn (a path or a name resolved on the child's PATH).
    pub command: String,
    /// Arguments, verbatim and in order.
    pub args: Vec<String>,
    /// Extra environment pairs for the child, after `${VAR}` expansion.
    pub env: BTreeMap<String, String>,
    /// The operator's trust declaration.
    pub trust: Trust,
}

/// `$HOME/.overseer/mcp.json`. `None` when the process has no HOME — the
/// caller then runs with no servers rather than guessing a location.
pub fn default_path() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(|home| PathBuf::from(home).join(".overseer").join("mcp.json"))
}

/// Load the configured servers from `path`, sorted by name (deterministic —
/// the prompt segment and the search output must not depend on file order).
///
/// A missing file is `Ok(empty)`: no config means no MCP, which is the
/// shipped state. Everything else that is wrong — unreadable file, malformed
/// JSON, a missing/blank `command`, a bad `args`/`env`/`trust` member, an
/// unset `${VAR}` — is an `Err` naming the server, the field and (for a
/// variable) the variable. Nothing is defaulted silently except the two
/// documented ones: `trust` defaults to `ask`, and absent `args`/`env` are
/// empty.
pub fn load(path: &Path) -> Result<Vec<McpServer>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => parse_with(&text, &|name| std::env::var(name).ok()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

/// [`load`]'s parser with the environment lookup injected, so the
/// `${VAR}` expansion is testable without mutating the process environment.
pub fn parse_with(
    text: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<Vec<McpServer>, String> {
    let doc: Value = serde_json::from_str(text).map_err(|e| format!("not valid JSON: {e}"))?;
    let top = doc
        .as_object()
        .ok_or("the top level must be an object like {\"mcpServers\": { … }}")?;
    let entries = match top.get("mcpServers") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Object(entries)) => entries,
        Some(other) => {
            return Err(format!(
                "`mcpServers` must be an object of servers, got {}",
                kind_of(other)
            ))
        }
    };
    let mut servers = Vec::with_capacity(entries.len());
    for (name, entry) in entries {
        if name.trim().is_empty() {
            return Err(
                "a server name must be non-blank (it namespaces the server's tools)".into(),
            );
        }
        let obj = entry
            .as_object()
            .ok_or_else(|| format!("server `{name}`: must be an object, got {}", kind_of(entry)))?;
        let command = match obj.get("command") {
            Some(Value::String(c)) if !c.trim().is_empty() => c.clone(),
            Some(Value::String(_)) => {
                return Err(format!("server `{name}`: `command` must not be blank"))
            }
            Some(other) => {
                return Err(format!(
                    "server `{name}`: `command` must be a string, got {}",
                    kind_of(other)
                ))
            }
            None => {
                return Err(format!(
                    "server `{name}`: missing required `command` (the program to spawn)"
                ))
            }
        };
        let args = match obj.get("args") {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(items)) => {
                let mut args = Vec::with_capacity(items.len());
                for (idx, item) in items.iter().enumerate() {
                    match item {
                        Value::String(s) => args.push(s.clone()),
                        other => {
                            return Err(format!(
                                "server `{name}`: `args[{idx}]` must be a string, got {}",
                                kind_of(other)
                            ))
                        }
                    }
                }
                args
            }
            Some(other) => {
                return Err(format!(
                    "server `{name}`: `args` must be an array of strings, got {}",
                    kind_of(other)
                ))
            }
        };
        let env = match obj.get("env") {
            None | Some(Value::Null) => BTreeMap::new(),
            Some(Value::Object(pairs)) => {
                let mut env = BTreeMap::new();
                for (key, value) in pairs {
                    let raw = match value {
                        Value::String(s) => s,
                        other => {
                            return Err(format!(
                                "server `{name}`: env `{key}` must be a string, got {}",
                                kind_of(other)
                            ))
                        }
                    };
                    env.insert(key.clone(), expand(raw, name, key, lookup)?);
                }
                env
            }
            Some(other) => {
                return Err(format!(
                    "server `{name}`: `env` must be an object of strings, got {}",
                    kind_of(other)
                ))
            }
        };
        let trust = match obj.get("trust") {
            None | Some(Value::Null) => Trust::Ask,
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "ask" => Trust::Ask,
                "read" => Trust::Read,
                other => {
                    return Err(format!(
                        "server `{name}`: `trust` must be \"read\" or \"ask\", got {other:?}"
                    ))
                }
            },
            Some(other) => {
                return Err(format!(
                    "server `{name}`: `trust` must be a string, got {}",
                    kind_of(other)
                ))
            }
        };
        servers.push(McpServer {
            name: name.clone(),
            command,
            args,
            env,
            trust,
        });
    }
    servers.sort_by(|a, b| a.name.cmp(&b.name));
    reject_ambiguous(&servers)?;
    Ok(servers)
}

/// Expand every `${VAR}` in `value` from `lookup`; an unset variable is an
/// error naming the server, the env key and the variable.
///
/// Only the braced form is recognised — `$VAR` (no braces) stays literal, so
/// a value that legitimately contains a bare `$` (a shell snippet a server
/// passes through) is not quietly rewritten. An unterminated `${` is also an
/// error: guessing where the name ends would silently corrupt the value.
fn expand(
    value: &str,
    server: &str,
    key: &str,
    lookup: &dyn Fn(&str) -> Option<String>,
) -> Result<String, String> {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            return Err(format!(
                "server `{server}`: env `{key}` has an unterminated `${{` — write `${{VAR}}`"
            ));
        };
        let var = &after[..end];
        if var.is_empty() {
            return Err(format!(
                "server `{server}`: env `{key}` has an empty `${{}}` — name the variable"
            ));
        }
        match lookup(var) {
            Some(expanded) => out.push_str(&expanded),
            None => {
                return Err(format!(
                    "server `{server}`: env `{key}` references `${{{var}}}`, which is not set in this process environment"
                ))
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Ok(out)
}

/// Two server ids whose sanitized forms can produce the *same*
/// `mcp__<server>__<tool>` name. The registry resolves a full name back to a
/// server by the sanitized prefix (longest match), and the gate decides
/// read-trust from that same segment, so an ambiguous pair would make both
/// decisions lie — refused at load instead, naming both servers.
///
/// Ambiguity is exact, not a guess: `san(b)` must extend `san(a)` by `_` or
/// by `__…`, because those are the only remainders that can be re-split as
/// `"__" + <tool>`. `github` / `github-enterprise` (remainder `_enterprise`)
/// is therefore left alone.
fn reject_ambiguous(servers: &[McpServer]) -> Result<(), String> {
    let ids: Vec<(String, String)> = servers
        .iter()
        .map(|s| (s.name.clone(), crate::mcp::sanitized(&s.name)))
        .collect();
    for (name_a, san_a) in &ids {
        for (name_b, san_b) in &ids {
            if name_a == name_b || !shadows(san_a, san_b) {
                continue;
            }
            return Err(format!(
                "servers `{name_a}` and `{name_b}` sanitize to `{san_a}` / `{san_b}`, where one is a \
                 prefix of the other on a namespace boundary — their tools would share names like \
                 `mcp__<server>__<tool>`; rename one"
            ));
        }
    }
    Ok(())
}

/// Whether `long` extends `short` by exactly the characters that can be
/// re-read as a separator (`_` alone, or a `__`-led remainder). Same
/// sanitized id counts.
fn shadows(short: &str, long: &str) -> bool {
    match long.strip_prefix(short) {
        None => false,
        Some("") => true,
        Some(rest) => rest == "_" || rest.starts_with("__"),
    }
}

/// A JSON value's type in words, for error messages.
fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "a boolean",
        Value::Number(_) => "a number",
        Value::String(_) => "a string",
        Value::Array(_) => "an array",
        Value::Object(_) => "an object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = r#"{
        "mcpServers": {
            "zeta": { "command": "mcp-zeta", "args": ["--x"], "trust": "read" },
            "alpha": { "command": "mcp-alpha", "env": { "TOKEN": "pre-${TOK}-post" } }
        }
    }"#;

    #[test]
    fn valid_file_parses_sorted_with_documented_defaults() {
        let servers = parse_with(VALID, &|name| (name == "TOK").then(|| "SECRET".to_string()))
            .expect("valid config parses");
        // Sorted by name, not file order.
        let names: Vec<&str> = servers.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "zeta"]);
        assert_eq!(servers[0].command, "mcp-alpha");
        assert!(servers[0].args.is_empty(), "absent args default to empty");
        assert_eq!(servers[0].env["TOKEN"], "pre-SECRET-post");
        assert_eq!(servers[0].trust, Trust::Ask, "trust defaults to ask");
        assert_eq!(servers[1].args, vec!["--x".to_string()]);
        assert_eq!(servers[1].trust, Trust::Read);
        assert_eq!(servers[1].env.len(), 0, "absent env defaults to empty");
        // A file with no servers key is not an error.
        assert!(parse_with("{}", &|_| None).unwrap().is_empty());
        assert!(parse_with(r#"{"mcpServers": null}"#, &|_| None)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn load_missing_file_is_empty_and_bad_json_names_the_file() {
        let dir = std::env::temp_dir().join(format!("overseer-mcp-cfg-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(load(&dir.join("nope.json")).unwrap().is_empty());
        std::fs::write(dir.join("bad.json"), "{ not json").unwrap();
        let err = load(&dir.join("bad.json")).unwrap_err();
        assert!(err.contains("not valid JSON"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_malformed_field_is_refused_by_name() {
        let cases: [(&str, &str); 8] = [
            (r#"{"mcpServers": []}"#, "mcpServers"),
            (r#"{"mcpServers": {"a": 5}}"#, "must be an object"),
            (r#"{"mcpServers": {"a": {}}}"#, "command"),
            (r#"{"mcpServers": {"a": {"command": "  "}}}"#, "command"),
            (
                r#"{"mcpServers": {"a": {"command": "x", "args": "y"}}}"#,
                "args",
            ),
            (
                r#"{"mcpServers": {"a": {"command": "x", "args": [1]}}}"#,
                "args[0]",
            ),
            (
                r#"{"mcpServers": {"a": {"command": "x", "env": {"K": 1}}}}"#,
                "env `K`",
            ),
            (
                r#"{"mcpServers": {"a": {"command": "x", "trust": "maybe"}}}"#,
                "trust",
            ),
        ];
        for (text, needle) in cases {
            let err = parse_with(text, &|_| None).unwrap_err();
            assert!(
                err.contains('a') && err.contains(needle),
                "{text} → {err} (want server + `{needle}`)"
            );
        }
        // A non-string trust and a blank server name are named too.
        let err = parse_with(
            r#"{"mcpServers": {"a": {"command": "x", "trust": 1}}}"#,
            &|_| None,
        )
        .unwrap_err();
        assert!(err.contains("trust"), "{err}");
        let err = parse_with(r#"{"mcpServers": {"": {"command": "x"}}}"#, &|_| None).unwrap_err();
        assert!(err.contains("server name"), "{err}");
    }

    #[test]
    fn unset_env_var_is_a_load_error_naming_the_var() {
        let text = r#"{"mcpServers": {"a": {"command": "x", "env": {"K": "${MISSING}"}}}}"#;
        let err = parse_with(text, &|_| None).unwrap_err();
        assert!(err.contains("MISSING"), "{err}");
        assert!(err.contains("not set"), "{err}");
        assert!(err.contains("env `K`"), "{err}");
        // A bare `$VAR` is literal, not an expansion request.
        let ok = parse_with(
            r#"{"mcpServers": {"a": {"command": "x", "env": {"K": "keep $VAR"}}}}"#,
            &|_| None,
        )
        .unwrap();
        assert_eq!(ok[0].env["K"], "keep $VAR");
        // An unterminated or empty braced form is refused.
        assert!(parse_with(
            r#"{"mcpServers": {"a": {"command": "x", "env": {"K": "${OPS"}}}}"#,
            &|_| None
        )
        .unwrap_err()
        .contains("unterminated"));
        assert!(parse_with(
            r#"{"mcpServers": {"a": {"command": "x", "env": {"K": "${}"}}}}"#,
            &|_| None
        )
        .unwrap_err()
        .contains("empty"));
    }

    #[test]
    fn ambiguous_server_ids_are_refused_and_distinct_ones_are_not() {
        let ambiguous = r#"{"mcpServers": {
            "a":   {"command": "x"},
            "a__b": {"command": "y"}
        }}"#;
        let err = parse_with(ambiguous, &|_| None).unwrap_err();
        assert!(err.contains("ambiguous") || err.contains("prefix"), "{err}");
        // A shared prefix that cannot be re-split as a separator is fine.
        let fine = r#"{"mcpServers": {
            "github": {"command": "x"},
            "github-enterprise": {"command": "y"}
        }}"#;
        let servers = parse_with(fine, &|_| None).unwrap();
        assert_eq!(servers.len(), 2);
    }

    #[test]
    fn default_path_is_home_dot_overseer() {
        let p = default_path().expect("the test environment has a HOME");
        assert!(p.ends_with(".overseer/mcp.json"), "{}", p.display());
    }
}
