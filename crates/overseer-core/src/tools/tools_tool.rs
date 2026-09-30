//! `tools` — the one resident dispatcher in front of the deferred tools.
//!
//! Rarely used tools (and every MCP tool) stay resident in the registry's
//! `base_specs` — modes, `--no-tools` and `check_args` see them exactly as
//! before — but are kept out of the advertised array. The model finds
//! them with `op=search` (schemas on demand) and runs them with
//! `op=call`, which re-enters [`ToolRegistry::call`] under the **inner**
//! name: disabled → unavailable → hooks → sensitive latch → `check_args`
//! → gate → run → post-hooks → `note_result` → sanitize → budget, all
//! keyed on the tool that actually executes. `tools` itself is a pure
//! dispatcher; the gate allows it and gates what it dispatches.
//!
//! MCP folds in: `mcp__<server>__<tool>` names re-enter the internal
//! `mcp` op tool (`{op:"call", tool, args}`), so the MCP trust lanes and
//! the untrusted latch apply unchanged.

use serde_json::{json, Value};

use super::{mcp_tool, schema, ToolCtx, ToolOutput, ToolRegistry};
use crate::provider::ToolSpec;

/// Resident names reached only through `tools`. `mcp` is the internal op
/// tool MCP names re-enter; it is never itself a `tools op=call` target.
pub const DEFERRED: [&str; 7] = [
    "computer",
    "diagnostics",
    "mcp",
    "plan",
    "repo_map",
    "struct_search",
    "symbol",
];

/// Catalog labels for the description, name-sorted by first member.
const LABELS: &[(&[&str], &str)] = &[
    (&["computer"], "GUI/browser control"),
    (&["diagnostics"], "compiler errors"),
    (&["plan"], "step list"),
    (&["repo_map", "symbol"], "code index"),
    (&["struct_search"], "AST/semgrep search"),
];

/// Most entries an empty-query listing prints.
const LIST_CAP: usize = 50;
/// Most schemas a ranked search returns.
const TOP_K: usize = 5;

/// The `tools` spec. `catalog` is the deferred names this registry can
/// actually reach; `mcp` whether MCP servers are reachable. A pure
/// function of both, so the bytes are stable for the session.
pub fn spec(catalog: &[&str], mcp: bool) -> ToolSpec {
    let mut behind: Vec<String> = LABELS
        .iter()
        .filter_map(|(names, label)| {
            let present: Vec<&str> = names
                .iter()
                .copied()
                .filter(|n| catalog.contains(n))
                .collect();
            (!present.is_empty()).then(|| format!("{} ({label})", present.join("/")))
        })
        .collect();
    if mcp {
        behind.push("MCP servers".to_string());
    }
    let behind = if behind.is_empty() {
        "none in this session".to_string()
    } else {
        behind.join(", ")
    };
    ToolSpec {
        name: "tools".into(),
        description: format!(
            "Find and call tools not listed here: {behind}. \
             op=search {{query}} → schemas; op=call {{name, args}}."
        ),
        input_schema: schema(
            json!({
                "op": {"type": "string", "enum": ["search", "call"]},
                "query": {"type": "string"},
                "name": {"type": "string"},
                "args": {"type": "object"}
            }),
            &["op"],
        ),
    }
}

/// The tool a call actually runs: `tools op=call {name, args}` unwraps to
/// `(name, args)`; anything else is returned as-is. For behaviour keyed
/// on a tool name outside the dispatch pipeline (audit, image siblings,
/// stuck detection, transcript labels). Events keep the outer call.
pub fn effective_call<'a>(name: &'a str, input: &'a Value) -> (&'a str, &'a Value) {
    static NO_ARGS: Value = Value::Null;
    if name == "tools" && op_is(input, "call") {
        if let Some(inner) = input.get("name").and_then(Value::as_str) {
            return (inner, input.get("args").unwrap_or(&NO_ARGS));
        }
    }
    (name, input)
}

/// Whether a `tools` input is `op == want` (ASCII case-insensitive).
pub fn op_is(input: &Value, want: &str) -> bool {
    input
        .get("op")
        .and_then(Value::as_str)
        .is_some_and(|op| op.trim().eq_ignore_ascii_case(want))
}

pub fn run(input: &Value, ctx: &mut ToolCtx, reg: &mut ToolRegistry) -> ToolOutput {
    if op_is(input, "search") {
        search(
            reg,
            input.get("query").and_then(Value::as_str).unwrap_or(""),
        )
    } else if op_is(input, "call") {
        call(input, ctx, reg)
    } else {
        ToolOutput::err("tools: `op` must be \"search\" or \"call\".")
    }
}

/// A deferred name the catalog may show: deferred, not the internal `mcp`
/// op tool, and not disabled (ablation or mode).
fn listed(reg: &ToolRegistry, name: &str) -> bool {
    name != "mcp" && reg.deferred.contains(name) && !reg.disabled.contains(name)
}

fn search(reg: &mut ToolRegistry, query: &str) -> ToolOutput {
    let mut entries: Vec<ToolSpec> = reg
        .base_specs
        .iter()
        .filter(|s| listed(reg, &s.name))
        .cloned()
        .collect();
    // An unavailable optional tool is shown with its reason and no schema.
    for (name, why) in &reg.unavailable {
        if listed(reg, name) {
            entries.push(ToolSpec {
                name: (*name).to_string(),
                description: format!("unavailable: {why}"),
                input_schema: Value::Null,
            });
        }
    }
    let mut notes = Vec::new();
    let mut from_mcp = false;
    if reg.mcp_reachable() {
        if let Some(state) = reg.mcp.as_mut() {
            notes = state.discover();
            for mut spec in state.discovered() {
                spec.description =
                    crate::mcp::trim_description(&spec.description, mcp_tool::DESC_CAP);
                entries.push(spec);
                from_mcp = true;
            }
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    let query = query.trim();
    let mut out = if query.is_empty() {
        list_all(&entries)
    } else {
        ranked(&entries, query)
    };
    for note in notes {
        out.push('\n');
        out.push_str(&note);
    }
    // Server-authored descriptions are untrusted content, exactly as an
    // `mcp op=search` result is: latch under the MCP name.
    if from_mcp {
        if let Some(notice) = reg
            .policy
            .note_result("mcp", &json!({"op": "search"}), &out)
        {
            reg.taint_notices.push(notice);
        }
    }
    ToolOutput::ok(out)
}

fn list_all(entries: &[ToolSpec]) -> String {
    let mut out = format!("tools: {} deferred tool(s)", entries.len());
    for spec in entries.iter().take(LIST_CAP) {
        let line = if spec.input_schema.is_null() {
            spec.description.clone()
        } else {
            first_sentence(&spec.description).to_string()
        };
        out.push_str(&format!("\n{} — {line}", spec.name));
    }
    if entries.len() > LIST_CAP {
        out.push_str(&format!(
            "\n… {} more — op=search with a query to narrow",
            entries.len() - LIST_CAP
        ));
    }
    out
}

fn ranked(entries: &[ToolSpec], query: &str) -> String {
    let hits = crate::mcp::search_tools(entries, query, TOP_K);
    if hits.is_empty() {
        return format!(
            "tools: no match for \"{query}\" — op=search with an empty query lists every \
             deferred tool"
        );
    }
    let mut out = format!("tools: {} match(es) for \"{query}\"", hits.len());
    for (name, _) in hits {
        let Some(spec) = entries.iter().find(|s| s.name == name) else {
            continue;
        };
        let line = if spec.input_schema.is_null() {
            json!({"name": spec.name, "unavailable": spec.description.trim_start_matches("unavailable: ")})
        } else {
            json!({
                "name": spec.name,
                "description": spec.description,
                "input_schema": spec.input_schema
            })
        };
        out.push('\n');
        out.push_str(&line.to_string());
    }
    out
}

/// Up to and including the first `.` followed by whitespace (or the end).
fn first_sentence(text: &str) -> &str {
    let text = text.trim();
    let cut = text
        .char_indices()
        .find(|&(i, c)| c == '.' && text[i + 1..].chars().next().is_none_or(char::is_whitespace))
        .map(|(i, _)| i + 1)
        .unwrap_or(text.len());
    text[..cut].lines().next().unwrap_or("")
}

fn call(input: &Value, ctx: &mut ToolCtx, reg: &mut ToolRegistry) -> ToolOutput {
    let Some(name) = input.get("name").and_then(Value::as_str) else {
        return ToolOutput::err("tools op=call needs `name` — a tool from op=search.");
    };
    if name == "tools" {
        return ToolOutput::err("tools cannot call tools — pass the inner tool's name.");
    }
    let args = input.get("args");
    if name.starts_with("mcp__") {
        return match args {
            Some(args) => reg.call(
                "mcp",
                &json!({"op": "call", "tool": name, "args": args}),
                ctx,
            ),
            None => missing_args_mcp(reg, name, ctx),
        };
    }
    if name == "mcp" || !reg.deferred.contains(name) {
        return ToolOutput::err(not_deferred(reg, name));
    }
    match args {
        Some(args) => reg.call(name, args, ctx),
        None => match reg.base_specs.iter().find(|s| s.name == name) {
            Some(spec) if !reg.disabled.contains(name) => ToolOutput::err(format!(
                "tools op=call `{name}` needs `args` matching its input_schema: {}",
                spec.input_schema
            )),
            // Disabled or unavailable: re-enter so the refusal is the
            // pipeline's own (and no schema leaks for a removed tool).
            _ => reg.call(name, &json!({}), ctx),
        },
    }
}

fn missing_args_mcp(reg: &mut ToolRegistry, name: &str, ctx: &mut ToolCtx) -> ToolOutput {
    if !reg.mcp_reachable() {
        return reg.call("mcp", &json!({"op": "call", "tool": name}), ctx);
    }
    match reg.mcp.as_mut().and_then(|state| state.schema_of(name)) {
        Some(schema) => ToolOutput::err(format!(
            "tools op=call `{name}` needs `args` matching its input_schema: {schema}"
        )),
        None => ToolOutput::err(format!(
            "tools: no MCP tool `{name}` — op=search lists the discovered tools."
        )),
    }
}

fn not_deferred(reg: &ToolRegistry, name: &str) -> String {
    if name == "mcp" {
        return "tools: call MCP tools by their full `mcp__<server>__<tool>` name from \
                op=search."
            .to_string();
    }
    if reg.specs.iter().any(|s| s.name == name) {
        return format!("tools: `{name}` is listed directly — call it as its own tool.");
    }
    let known: Vec<String> = reg
        .base_specs
        .iter()
        .map(|s| s.name.clone())
        .filter(|n| listed(reg, n))
        .collect();
    let mut msg = format!("tools: no deferred tool `{name}` — op=search lists them.");
    if let Some(hint) = crate::fuzzy::miss_hint(name, &known, 3) {
        msg.push(' ');
        msg.push_str(&hint);
    }
    msg
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::tools::{Optional, TOOL_NAMES};

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-tools-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&d).unwrap();
        d
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

    fn full() -> ToolRegistry {
        ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::ALL)
    }

    fn spec_chars(s: &ToolSpec) -> usize {
        json!({"name": s.name, "description": s.description, "input_schema": s.input_schema})
            .to_string()
            .len()
    }

    #[test]
    fn spec_names_only_what_is_reachable_and_fits_its_budget() {
        let reg = full();
        let tools = reg.specs.iter().find(|s| s.name == "tools").unwrap();
        assert_eq!(
            tools.description,
            "Find and call tools not listed here: computer (GUI/browser control), \
             diagnostics (compiler errors), plan (step list), repo_map/symbol (code index), \
             struct_search (AST/semgrep search). op=search {query} → schemas; \
             op=call {name, args}."
        );
        assert!(!tools.description.contains("MCP"), "no MCP configured");
        let with_mcp = spec(&DEFERRED, true);
        assert!(with_mcp.description.contains(", MCP servers."));
        assert!(spec_chars(&with_mcp) <= 600, "{}", spec_chars(&with_mcp));

        // An ablated deferred tool drops out of the description too.
        let mut reg = full();
        reg.disable(&["computer".to_string(), "symbol".to_string()]);
        let tools = reg.specs.iter().find(|s| s.name == "tools").unwrap();
        assert!(!tools.description.contains("computer"));
        assert!(tools.description.contains("repo_map (code index)"));
    }

    #[test]
    fn deferred_catalog_holds_every_deferred_tool_and_the_advertised_array_none() {
        let mut reg = full();
        for name in DEFERRED {
            assert!(TOOL_NAMES.contains(&name), "{name} is a --no-tools name");
            assert!(
                !reg.specs.iter().any(|s| s.name == name),
                "{name} must not be advertised"
            );
            if name != "mcp" {
                assert!(reg.base_specs.iter().any(|s| s.name == name), "{name}");
            }
        }
        let dir = tmpdir();
        let listing = reg.call("tools", &json!({"op": "search"}), &mut ctx(&dir));
        for name in DEFERRED.iter().filter(|n| **n != "mcp") {
            assert!(
                listing
                    .text
                    .lines()
                    .any(|l| l.starts_with(&format!("{name} — "))),
                "{name} missing from: {}",
                listing.text
            );
        }
        assert!(!listing.text.lines().any(|l| l.starts_with("mcp — ")));
    }

    #[test]
    fn search_ranks_returns_schemas_and_respects_disabled_and_unavailable() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        let mut reg = full();
        let out = reg.call("tools", &json!({"op": "search", "query": "struct"}), &mut c);
        assert!(!out.is_error, "{}", out.text);
        let first: Value = serde_json::from_str(out.text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(first["name"], "struct_search", "prefix match ranks first");
        assert_eq!(first["input_schema"]["additionalProperties"], false);

        reg.disable(&["struct_search".to_string()]);
        let out = reg.call("tools", &json!({"op": "search", "query": "struct"}), &mut c);
        assert!(!out.text.contains("struct_search"), "{}", out.text);
        let listing = reg.call("tools", &json!({"op": "search", "query": ""}), &mut c);
        assert!(!listing.text.contains("struct_search"), "{}", listing.text);

        let none = Optional {
            computer: false,
            struct_search: false,
            diagnostics: false,
            skill: false,
        };
        let mut bare = ToolRegistry::core_with(crate::perm::Policy::allow_all(), none);
        let listing = bare.call("tools", &json!({"op": "search", "query": ""}), &mut c);
        assert!(
            listing
                .text
                .lines()
                .any(|l| l.starts_with("computer — unavailable: ")),
            "{}",
            listing.text
        );
        let called = bare.call(
            "tools",
            &json!({"op": "call", "name": "computer", "args": {"action": "apps"}}),
            &mut c,
        );
        assert!(
            called.is_error && called.text.contains("not available"),
            "{}",
            called.text
        );
    }

    #[test]
    fn call_reenters_under_the_inner_name() {
        let dir = tmpdir();
        std::fs::write(dir.join("lib.rs"), "pub fn alpha() {}\n").unwrap();
        let mut c = ctx(&dir);
        let mut reg = full();
        let out = reg.call(
            "tools",
            &json!({"op": "call", "name": "plan", "args": {"items": [{"text": "a"}]}}),
            &mut c,
        );
        let direct = full().call("plan", &json!({"items": [{"text": "a"}]}), &mut ctx(&dir));
        assert_eq!(out.is_error, direct.is_error, "{}", out.text);

        // check_args runs against the inner schema.
        let bad = reg.call(
            "tools",
            &json!({"op": "call", "name": "symbol", "args": {"name": "alpha", "bogus": 1}}),
            &mut c,
        );
        assert!(bad.is_error && bad.text.contains("bogus"), "{}", bad.text);
        // Without args, the inner schema comes back in the error.
        let bare = reg.call("tools", &json!({"op": "call", "name": "symbol"}), &mut c);
        assert!(
            bare.is_error && bare.text.contains("input_schema"),
            "{}",
            bare.text
        );
    }

    #[test]
    fn call_refuses_resident_recursive_and_unknown_names() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        let mut reg = full();
        let resident = reg.call(
            "tools",
            &json!({"op": "call", "name": "read", "args": {"path": "x"}}),
            &mut c,
        );
        assert!(resident.is_error && resident.text.contains("call it as its own tool"));
        let recursive = reg.call(
            "tools",
            &json!({"op": "call", "name": "tools", "args": {"op": "search"}}),
            &mut c,
        );
        assert!(recursive.is_error && recursive.text.contains("cannot call tools"));
        let internal = reg.call(
            "tools",
            &json!({"op": "call", "name": "mcp", "args": {"op": "search"}}),
            &mut c,
        );
        assert!(internal.is_error && internal.text.contains("mcp__<server>__<tool>"));
        let typo = reg.call(
            "tools",
            &json!({"op": "call", "name": "symbl", "args": {}}),
            &mut c,
        );
        assert!(
            typo.is_error && typo.text.contains("symbol"),
            "{}",
            typo.text
        );
        let op = reg.call("tools", &json!({"op": "list"}), &mut c);
        assert!(op.is_error, "{}", op.text);
    }

    #[test]
    fn a_mode_that_removes_computer_makes_tools_refuse_it() {
        let dir = tmpdir();
        let mut c = ctx(&dir);
        let mut reg = full();
        reg.set_mode(crate::modes::for_mode("ask").unwrap(), &[]);
        assert!(reg.specs.iter().any(|s| s.name == "tools"));
        let out = reg.call(
            "tools",
            &json!({"op": "call", "name": "computer", "args": {"action": "apps"}}),
            &mut c,
        );
        assert!(
            out.is_error && out.text.contains("disabled"),
            "{}",
            out.text
        );
        let search = reg.call(
            "tools",
            &json!({"op": "search", "query": "computer"}),
            &mut c,
        );
        assert!(
            !search.text.contains("\"name\":\"computer\""),
            "{}",
            search.text
        );
    }

    #[test]
    fn computer_through_tools_is_gated_and_latched_like_a_direct_call() {
        let dir = tmpdir();
        for action in ["screenshot", "click", "apps"] {
            let args = json!({"action": action, "x": 1, "y": 1});
            let policy = || crate::perm::Policy::headless(dir.clone());
            let mut direct = ToolRegistry::core_with(policy(), Optional::ALL);
            let mut via = ToolRegistry::core_with(policy(), Optional::ALL);
            let d = direct.call("computer", &args, &mut ctx(&dir));
            let v = via.call(
                "tools",
                &json!({"op": "call", "name": "computer", "args": args}),
                &mut ctx(&dir),
            );
            assert_eq!(
                (d.denied, d.is_error, &d.text),
                (v.denied, v.is_error, &v.text),
                "{action}"
            );
            assert_eq!(direct.taint_notices, via.taint_notices, "{action}");
        }
        // The screenshot-context latch fires under the inner name.
        let mut via =
            ToolRegistry::core_with(crate::perm::Policy::headless(dir.clone()), Optional::ALL);
        via.call(
            "tools",
            &json!({"op": "call", "name": "computer", "args": {"action": "screenshot"}}),
            &mut ctx(&dir),
        );
        assert!(
            via.taint_notices.iter().any(|n| n.contains("via computer")),
            "{:?}",
            via.taint_notices
        );
    }

    #[test]
    fn read_only_presets_allow_the_dispatcher_but_gate_the_inner_call() {
        let dir = tmpdir();
        let policy = crate::perm::Policy::preset(crate::perm::Preset::ReadOnly, dir.clone());
        let mut reg = ToolRegistry::core_with(policy, Optional::ALL);
        let search = reg.call(
            "tools",
            &json!({"op": "search", "query": "plan"}),
            &mut ctx(&dir),
        );
        assert!(!search.is_error && !search.denied, "{}", search.text);
        let click = reg.call(
            "tools",
            &json!({"op": "call", "name": "computer", "args": {"action": "click", "x": 1, "y": 1}}),
            &mut ctx(&dir),
        );
        assert!(click.denied, "{}", click.text);
        assert!(click.text.contains("computer: denied"), "{}", click.text);
    }

    #[test]
    fn effective_call_unwraps_only_tools_call() {
        let wrapped = json!({"op": "call", "name": "computer", "args": {"action": "click"}});
        assert_eq!(
            effective_call("tools", &wrapped),
            ("computer", &json!({"action": "click"}))
        );
        let search = json!({"op": "search", "query": "x"});
        assert_eq!(effective_call("tools", &search), ("tools", &search));
        let direct = json!({"name": "computer"});
        assert_eq!(effective_call("read", &direct), ("read", &direct));
    }
}
