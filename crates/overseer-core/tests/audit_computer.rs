//! Audit: computer use against a scripted, hostile fake cua-driver.
//!
//! The fake driver is this test binary re-entered through a tiny shell
//! wrapper (`fake_driver_entry`): it speaks newline-delimited MCP JSON-RPC on
//! fd 3 (libtest's own stdout is sent to /dev/null), validates every
//! `tools/call` against the cua-driver 0.34 macOS tool schemas
//! (trycua/cua `libs/cua-driver/rust`, closed schemas admit `session`), and
//! can misbehave per mode. Every request is appended to a per-test log.

use overseer_core::agent::{Agent, AgentConfig};
use overseer_core::control::Control;
use overseer_core::event::{Event, EventKind};
use overseer_core::ir::{Block, Usage};
use overseer_core::perm::{classify, Irreversibility, Policy};
use overseer_core::provider::{Provider, ProviderError, Request, Response, StopReason};
use overseer_core::tools::computer::{self, Backends, ComputerState};
use overseer_core::tools::task::SubagentCtx;
use overseer_core::tools::{Optional, ToolCtx, ToolRegistry};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- fake driver

/// Not a test: the fake driver's entry point when re-entered via a wrapper.
#[test]
fn fake_driver_entry() {
    let Ok(mode) = std::env::var("AUDIT_FAKE_MODE") else {
        return;
    };
    let log = PathBuf::from(std::env::var("AUDIT_FAKE_LOG").unwrap_or_default());
    serve(&mode, &log);
    std::process::exit(0);
}

fn log_to(log: &Path, line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(f, "{line}");
    }
}

fn serve(mode: &str, log: &Path) {
    use std::os::fd::FromRawFd;
    // SAFETY: the wrapper dup'd the MCP stdout pipe onto fd 3.
    let mut out = unsafe { std::fs::File::from_raw_fd(3) };
    log_to(log, &format!("spawn {mode}"));
    if mode == "preamble" {
        let _ = writeln!(out, "cua-driver 0.34.0: connecting to daemon...");
        let _ = out.flush();
    }
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        log_to(log, &format!("req {line}"));
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let method = msg["method"].as_str().unwrap_or("").to_string();
        let result = match method.as_str() {
            "initialize" => {
                if mode == "crash_init" {
                    std::process::exit(1);
                }
                json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                       "serverInfo": {"name": "audit-fake-cua-driver", "version": "0.34.0"}})
            }
            m if m.starts_with("notifications/") => continue,
            "tools/list" => json!({"tools": []}),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("").to_string();
                let args = msg["params"]["arguments"].clone();
                match mode {
                    "crash_call" => std::process::exit(3),
                    "slow" => std::thread::sleep(Duration::from_secs(40)),
                    "slow_end" if name == "end_session" => {
                        std::thread::sleep(Duration::from_secs(40))
                    }
                    "stderr_flood" => {
                        let chunk = vec![b'E'; 1 << 20];
                        let mut e = std::io::stderr();
                        for _ in 0..16 {
                            let _ = e.write_all(&chunk);
                        }
                    }
                    "garbage" => {
                        let _ = writeln!(out, "this is not json");
                        let _ = out.flush();
                        continue;
                    }
                    _ => {}
                }
                handle(mode, &name, &args)
            }
            _ => {
                let _ = writeln!(
                    out,
                    "{}",
                    json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "no such method"}})
                );
                let _ = out.flush();
                continue;
            }
        };
        let id_out = if mode == "wrong_id" && method == "tools/call" {
            json!(id.as_i64().unwrap_or(0) + 1000)
        } else {
            id
        };
        let _ = writeln!(
            out,
            "{}",
            json!({"jsonrpc": "2.0", "id": id_out, "result": result})
        );
        let _ = out.flush();
    }
}

/// `(required, Some(closed property set) | None for open)` — cua-driver
/// 0.34 macOS `tools/list` (platform-macos/src/tools/*.rs, contract inputs).
fn schema(tool: &str) -> Option<(Vec<&'static str>, Option<Vec<&'static str>>)> {
    let s = |r: &[&'static str], p: &[&'static str]| Some((r.to_vec(), Some(p.to_vec())));
    match tool {
        "list_apps" => s(&[], &[]),
        "list_windows" => s(&[], &["on_screen_only", "pid"]),
        "get_window_state" => s(
            &["pid", "window_id"],
            &[
                "capture_mode",
                "include_accessibility_tree",
                "include_screenshot",
                "max_depth",
                "max_dimension",
                "max_elements",
                "max_image_dimension",
                "pid",
                "query",
                "screenshot_out_file",
                "timeout_ms",
                "window_id",
            ],
        ),
        "zoom" => s(
            &["window_id", "x1", "y1", "x2", "y2"],
            &["pid", "window_id", "x1", "y1", "x2", "y2"],
        ),
        "click" => s(
            &[],
            &[
                "action",
                "button",
                "capture_id",
                "count",
                "debug_image_out",
                "delivery_mode",
                "element_token",
                "from_zoom",
                "modifier",
                "pid",
                "scope",
                "window_id",
                "x",
                "y",
            ],
        ),
        "right_click" => s(
            &["pid"],
            &[
                "delivery_mode",
                "element_token",
                "modifier",
                "pid",
                "window_id",
                "x",
                "y",
            ],
        ),
        "double_click" => s(&["pid"], &["element_token", "pid", "x", "y", "window_id"]),
        "type_text" => s(
            &["text"],
            &[
                "delay_ms",
                "delivery_mode",
                "element_token",
                "pid",
                "scope",
                "text",
                "window_id",
                "x",
                "y",
            ],
        ),
        "press_key" => s(
            &["key"],
            &[
                "delivery_mode",
                "element_token",
                "key",
                "modifiers",
                "pid",
                "scope",
                "window_id",
                "x",
                "y",
            ],
        ),
        "hotkey" => s(
            &["keys"],
            &[
                "delivery_mode",
                "element_token",
                "keys",
                "pid",
                "scope",
                "window_id",
                "x",
                "y",
            ],
        ),
        "set_value" => s(
            &["pid", "value"],
            &["element_token", "pid", "value", "window_id"],
        ),
        "scroll" => s(
            &["direction"],
            &[
                "amount",
                "by",
                "delivery_mode",
                "direction",
                "element_token",
                "pid",
                "scope",
                "window_id",
                "x",
                "y",
            ],
        ),
        "drag" => s(
            &["from_x", "from_y", "to_x", "to_y"],
            &[
                "button",
                "delivery_mode",
                "duration_ms",
                "from_x",
                "from_y",
                "from_zoom",
                "modifier",
                "pid",
                "scope",
                "steps",
                "to_x",
                "to_y",
                "window_id",
            ],
        ),
        "invoke_menu" => s(&["pid", "window_id", "path"], &["path", "pid", "window_id"]),
        "launch_app" => s(
            &[],
            &[
                "additional_arguments",
                "bundle_id",
                "creates_new_application_instance",
                "name",
                "urls",
                "webkit_inspector_port",
            ],
        ),
        "verify_state" => s(
            &["pid", "window_id", "expect"],
            &["expect", "pid", "window_id"],
        ),
        "end_session" => s(&[], &[]),
        "check_permissions" => s(&[], &["probe_direct_capture", "prompt"]),
        "get_browser_state" => Some((vec![], None)),
        "browser_click" => Some((vec!["target_id", "tab_id"], None)),
        "browser_type" => Some((vec!["target_id", "tab_id", "ref", "text"], None)),
        "browser_navigate" => Some((vec!["target_id", "tab_id", "url"], None)),
        _ => None,
    }
}

fn schema_violation(tool: &str, args: &Value) -> Option<String> {
    let Some((required, props)) = schema(tool) else {
        return Some(format!("unknown tool {tool}"));
    };
    let obj = args.as_object().cloned().unwrap_or_default();
    for r in required {
        if !obj.contains_key(r) {
            return Some(format!("{tool}: missing required argument {r}"));
        }
    }
    if let Some(props) = props {
        for k in obj.keys() {
            if k != "session" && !props.contains(&k.as_str()) {
                return Some(format!("{tool}: unknown argument {k}"));
            }
        }
    }
    let len = |k: &str| obj.get(k).and_then(Value::as_array).map(Vec::len);
    match tool {
        "hotkey" if len("keys").unwrap_or(0) < 2 => {
            Some("hotkey: keys must contain at least 2 items".into())
        }
        "verify_state" if !(1..=8).contains(&len("expect").unwrap_or(0)) => {
            Some("verify_state: expect must hold 1 to 8 predicates".into())
        }
        "invoke_menu" if !(1..=16).contains(&len("path").unwrap_or(0)) => {
            Some("invoke_menu: path must hold 1 to 16 labels".into())
        }
        "scroll"
            if obj
                .get("amount")
                .and_then(Value::as_i64)
                .is_some_and(|a| !(1..=50).contains(&a)) =>
        {
            Some("scroll: amount must be 1..=50".into())
        }
        _ => None,
    }
}

fn refusal(code: &str, why: &str) -> Value {
    json!({"isError": true,
           "content": [{"type": "text", "text": format!("refused ({code}): {why}")}],
           "structuredContent": {"status": "refused", "refusal": {"code": code, "message": why}}})
}

const ACT_TOOLS: &[&str] = &[
    "click",
    "right_click",
    "double_click",
    "type_text",
    "press_key",
    "hotkey",
    "set_value",
    "scroll",
    "drag",
    "invoke_menu",
    "launch_app",
];

fn image(data: String, w: u64, h: u64, extra: Value) -> Value {
    let mut sc = json!({"width": w, "height": h, "screenshot_width": w, "screenshot_height": h,
                        "window_bounds": {"width": w, "height": h}, "snapshot_id": "snap-img"});
    if let (Some(o), Some(e)) = (sc.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    json!({"content": [{"type": "text", "text": "captured"},
                       {"type": "image", "data": data, "mimeType": "image/png"}],
           "structuredContent": sc})
}

fn handle(mode: &str, name: &str, args: &Value) -> Value {
    if let Some(why) = schema_violation(name, args) {
        return refusal("invalid_arguments", &why);
    }
    if mode == "slow_acts" && ACT_TOOLS.contains(&name) {
        std::thread::sleep(Duration::from_millis(150));
    }
    match name {
        "list_apps" if mode == "oversized" => {
            json!({"content": [{"type": "text", "text": "A".repeat(20 << 20)}]})
        }
        "list_apps" => json!({"content": [{"type": "text", "text": "1 app"}],
            "structuredContent": {"apps": [{"pid": 11, "name": "TextEdit", "running": true, "active": true}]}}),
        "list_windows" => json!({"content": [{"type": "text", "text": "1 window"}],
            "structuredContent": {"windows": [{"window_id": 101, "pid": 11, "app_name": "TextEdit",
                "title": "Untitled", "bounds": {"width": 800, "height": 600}}]}}),
        "get_window_state" if args["include_accessibility_tree"] == json!(false) => {
            let max = args["max_dimension"].as_u64().unwrap_or(1280);
            let data = if mode == "big_image" {
                "A".repeat(12 << 20)
            } else {
                "iVBORw0KGgo=".into()
            };
            image(data, max, max * 5 / 8, json!({}))
        }
        "get_window_state" => {
            let n = if mode == "huge_tree" { 100_000 } else { 3 };
            let els: Vec<Value> = (0..n)
                .map(|i| {
                    json!({"element_index": i, "element_token": format!("tok-{i}"),
                           "role": if i == 0 { "AXWindow" } else { "AXButton" },
                           "label": format!("Button {i}"), "actions": ["AXPress"],
                           "depth": if i == 0 { 0 } else { 1 },
                           "parent_index": if i == 0 { Value::Null } else { json!(0) }})
                })
                .collect();
            json!({"content": [{"type": "text", "text": "tree"}],
                   "structuredContent": {"snapshot_id": "snap-1", "elements": els,
                                         "element_count": n}})
        }
        "zoom" => image("iVBORw0KGgo=".into(), 500, 300, json!({})),
        "verify_state" => json!({"content": [{"type": "text", "text": "verified"}],
            "structuredContent": {"status": "satisfied", "predicates": []}}),
        "get_browser_state" => json!({"content": [{"type": "text", "text": "tab"}],
            "structuredContent": {"target_id": "t-1", "tab_id": "tab-1",
                "refs": [{"ref": "p1:0", "role": "button", "name": "Go"}]}}),
        _ => json!({"content": [{"type": "text", "text": format!("done {name}")}]}),
    }
}

// ------------------------------------------------------------- test harness

/// Every (tag, mode) wrapper is written once, before any test spawns a
/// child, so no fork can inherit a writable fd to a script (ETXTBSY).
const FAKES: &[(&str, &str)] = &[
    ("env", "slow_acts"),
    ("label", "normal"),
    ("schema", "normal"),
    ("preamble", "preamble"),
    ("wrong_id", "wrong_id"),
    ("garbage", "garbage"),
    ("crash_call", "crash_call"),
    ("crash_init", "crash_init"),
    ("slow", "slow"),
    ("slow_end", "slow_end"),
    ("stderr_flood", "stderr_flood"),
    ("oversized", "oversized"),
    ("huge_tree", "huge_tree"),
    ("loop", "normal"),
    ("big_image", "big_image"),
    ("coords", "normal"),
    ("cleanup", "normal"),
    ("fallback", "crash_init"),
];

struct Fake {
    path: PathBuf,
    log: PathBuf,
}

fn fakes() -> &'static HashMap<&'static str, Fake> {
    static F: OnceLock<HashMap<&'static str, Fake>> = OnceLock::new();
    F.get_or_init(|| {
        use std::os::unix::fs::PermissionsExt;
        let exe = std::env::current_exe().unwrap();
        let root = tmp("fakes");
        let mut m = HashMap::new();
        for (tag, mode) in FAKES {
            let dir = root.join(tag);
            std::fs::create_dir_all(&dir).unwrap();
            let log = dir.join("driver.log");
            let path = dir.join("cua-driver");
            let script = format!(
                "#!/bin/sh\nexport AUDIT_FAKE_MODE='{mode}'\nexport AUDIT_FAKE_LOG='{}'\n\
                 exec 3>&1 1>/dev/null\nexec '{}' --exact fake_driver_entry --nocapture --test-threads=1 -q\n",
                log.display(),
                exe.display()
            );
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(script.as_bytes()).unwrap();
            f.sync_all().unwrap();
            drop(f);
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            m.insert(*tag, Fake { path, log });
        }
        // Process-wide driver for registry/agent tests (detect() reads env).
        std::env::set_var("OVERSEER_COMPUTER_DRIVER", &m["env"].path);
        // A logging pixel helper for the fallback test.
        let helper = root.join("pixel-helper");
        let hlog = root.join("pixel-helper.log");
        std::fs::write(
            &helper,
            format!(
                "#!/bin/sh\necho called >> '{}'\necho '{{\"ok\":true}}'\n",
                hlog.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
        m.insert(
            "pixel-helper",
            Fake {
                path: helper,
                log: hlog,
            },
        );
        m
    })
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "overseer-audit-computer-{tag}-{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A session dir whose name gives a unique 8-char `ovs-` label.
fn session(tag: &str) -> PathBuf {
    let id = uuid::Uuid::now_v7().simple().to_string();
    let name: String = id.chars().rev().take(8).collect();
    let d = tmp(tag).join(name);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn label_of(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let path = dir.canonicalize().unwrap();
    let hex = format!("{:x}", Sha256::digest(path.as_os_str().as_encoded_bytes()));
    format!("ovs-{}", &hex[..8])
}

fn ctx(dir: &Path) -> ToolCtx<'static> {
    ToolCtx {
        cwd: dir.to_path_buf(),
        session_dir: dir.to_path_buf(),
        spill_seq: 0,
        provider: None,
        agent_config: None,
        subagents: SubagentCtx {
            control: Default::default(),
            seq: 0,
            spend: None,
        },
        checkpoint: None,
        sandbox: false,
        broker: None,
    }
}

fn state(tag: &str) -> ComputerState {
    ComputerState::new(Backends {
        driver: Some(fakes()[tag].path.clone()),
        ..Default::default()
    })
}

fn run(input: Value, c: &mut ToolCtx, st: &mut ComputerState) -> Result<Value, String> {
    computer::run_with(&input, c, st)
}

/// `(tool, arguments)` of every `tools/call` in a log, optionally only for
/// one session label.
fn calls(tag: &str, label: Option<&str>) -> Vec<(String, Value)> {
    let text = std::fs::read_to_string(&fakes()[tag].log).unwrap_or_default();
    text.lines()
        .filter_map(|l| l.strip_prefix("req "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["method"] == "tools/call")
        .map(|m| {
            (
                m["params"]["name"].as_str().unwrap_or("").to_string(),
                m["params"]["arguments"].clone(),
            )
        })
        .filter(|(_, a)| label.is_none_or(|l| a["session"] == json!(l)))
        .collect()
}

fn spawns(tag: &str) -> usize {
    std::fs::read_to_string(&fakes()[tag].log)
        .unwrap_or_default()
        .lines()
        .filter(|l| l.starts_with("spawn "))
        .count()
}

fn win() -> Value {
    json!({"pid": 11, "window_id": 101})
}

fn with(base: Value, extra: Value) -> Value {
    let mut b = base;
    b.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    b
}

fn registry() -> ToolRegistry {
    let _ = fakes();
    ToolRegistry::core_with(Policy::headless(std::env::temp_dir()), Optional::ALL)
}

// =================================================================== findings

/// The CLI names a session dir by its millisecond timestamp; the label
/// takes its first 8 chars, so every session started in the same 100 s
/// window drives the same cua-driver session.
#[test]
fn two_cli_sessions_started_75s_apart_get_distinct_driver_labels() {
    let root = tmp("label");
    let a = root.join("1791298020123");
    let b = root.join("1791298095876");
    for d in [&a, &b] {
        std::fs::create_dir_all(d).unwrap();
        let mut st = state("label");
        run(json!({"action": "apps"}), &mut ctx(d), &mut st).unwrap();
    }
    let labels: Vec<Value> = calls("label", None)
        .into_iter()
        .filter(|(t, _)| t == "list_apps")
        .map(|(_, a)| a["session"].clone())
        .collect();
    assert_eq!(labels.len(), 2);
    assert_ne!(
        labels[0], labels[1],
        "two sessions share driver session {}",
        labels[0]
    );
}

/// Subagent session dirs are `<parent>/subagents/task-<n>`: every
/// session's first subagent drives the same `ovs-task-1` driver session.
#[test]
fn first_subagents_of_two_sessions_get_distinct_driver_labels() {
    let root = tmp("label-sub");
    let a = root.join("1791298020123/subagents/task-1");
    let b = root.join("1791399999999/subagents/task-1");
    let mut seen = Vec::new();
    for d in [&a, &b] {
        std::fs::create_dir_all(d).unwrap();
        let mut st = state("label");
        run(json!({"action": "windows"}), &mut ctx(d), &mut st).unwrap();
        seen.push(
            calls("label", None)
                .into_iter()
                .rfind(|(t, _)| t == "list_windows")
                .unwrap()
                .1["session"]
                .clone(),
        );
    }
    assert_ne!(seen[0], seen[1], "both subagents drive {}", seen[0]);
}

/// After `zoom`, a right-click / double-click by x,y is refused locally:
/// the driver's 0.34 schemas for those tools carry no `from_zoom`, and the
/// crop padding can't be mapped back here. A plain click still rides
/// `from_zoom`.
#[test]
fn zoom_then_right_and_double_click_are_refused_locally() {
    const REFUSAL: &str = "re-take a full screenshot before right/double click";
    let dir = session("zoom");
    let label = label_of(&dir);
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    run(
        with(win(), json!({"action": "screenshot"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    run(
        with(
            win(),
            json!({"action": "zoom", "x1": 0, "y1": 0, "x2": 200, "y2": 120}),
        ),
        &mut c,
        &mut st,
    )
    .unwrap();
    let right = run(
        with(
            win(),
            json!({"action": "click", "x": 40, "y": 30, "button": "right"}),
        ),
        &mut c,
        &mut st,
    );
    let double = run(
        with(
            win(),
            json!({"action": "click", "x": 40, "y": 30, "count": 2}),
        ),
        &mut c,
        &mut st,
    );
    for (what, r) in [("right-click", &right), ("double-click", &double)] {
        let err = r
            .as_ref()
            .expect_err(&format!("{what} after zoom was not refused"));
        assert!(err.contains(REFUSAL), "{what} after zoom: {err}");
    }
    let click = run(
        with(win(), json!({"action": "click", "x": 40, "y": 30})),
        &mut c,
        &mut st,
    );
    assert!(click.is_ok(), "plain click after zoom: {click:?}");
    let sent = calls("schema", Some(&label));
    let zoomed: Vec<&(String, Value)> = sent
        .iter()
        .filter(|(t, a)| {
            matches!(t.as_str(), "right_click" | "double_click") && a.get("from_zoom").is_some()
        })
        .collect();
    assert!(
        zoomed.is_empty(),
        "driver got right/double click with from_zoom: {zoomed:?}"
    );
    assert!(
        sent.iter()
            .any(|(t, a)| t == "click" && a["from_zoom"] == json!(true)),
        "plain click after zoom sent no from_zoom: {sent:?}"
    );
}

/// `browser_type` needs `ref` in the driver schema; the tool's own
/// validation lets it through and the driver refusal is reworded as
/// "use observe/click on the window instead".
#[test]
fn browser_type_without_ref_is_refused_before_dispatch() {
    let dir = session("btype");
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    run(with(win(), json!({"action": "browser"})), &mut c, &mut st).unwrap();
    let tab = "tab-1";
    let r = run(
        json!({"action": "browser_type", "tab": tab, "text": "hello"}),
        &mut c,
        &mut st,
    );
    let sent = calls("schema", Some(&label_of(&dir)))
        .into_iter()
        .any(|(t, _)| t == "browser_type");
    assert!(r.is_err());
    assert!(
        !sent,
        "browser_type without ref reached the driver; model saw: {r:?}"
    );
}

/// An element index absent from the remembered tokens falls back to
/// `element_index`/`snapshot_id`, which the 0.34 click schema refuses as
/// unknown arguments (not "stale — observe again").
#[test]
fn click_on_unremembered_element_never_sends_unknown_driver_arguments() {
    let dir = session("elfb");
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    run(with(win(), json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let r = run(
        with(win(), json!({"action": "click", "element": 7})),
        &mut c,
        &mut st,
    );
    let bad: Vec<Value> = calls("schema", Some(&label_of(&dir)))
        .into_iter()
        .filter(|(t, a)| t == "click" && a.get("element_index").is_some())
        .map(|(_, a)| a)
        .collect();
    assert!(bad.is_empty(), "sent {bad:?}; model saw {r:?}");
}

/// Off-frame coordinates (another display, a stale frame) are clamped onto
/// the window edge and a real click lands there.
#[test]
fn off_frame_click_is_refused_not_clamped_onto_the_edge() {
    let dir = session("coords");
    let (mut c, mut st) = (ctx(&dir), state("coords"));
    run(
        with(win(), json!({"action": "screenshot"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    let r = run(
        with(win(), json!({"action": "click", "x": 5000, "y": -40})),
        &mut c,
        &mut st,
    );
    let clicks: Vec<Value> = calls("coords", Some(&label_of(&dir)))
        .into_iter()
        .filter(|(t, _)| t == "click")
        .map(|(_, a)| json!([a["x"], a["y"]]))
        .collect();
    assert!(
        r.is_err() && clicks.is_empty(),
        "off-frame click dispatched at {clicks:?}: {r:?}"
    );
}

/// `max` is clamped to 8192 and forwarded as `max_dimension`; Anthropic's
/// Messages API rejects images over 8000 px on an edge.
#[test]
fn screenshot_max_is_capped_to_the_provider_image_limit() {
    let dir = session("maxdim");
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    run(
        with(win(), json!({"action": "screenshot", "max": 8192})),
        &mut c,
        &mut st,
    )
    .unwrap();
    let sent = calls("schema", Some(&label_of(&dir)))
        .into_iter()
        .find(|(t, a)| t == "get_window_state" && a["include_accessibility_tree"] == json!(false))
        .map(|(_, a)| a["max_dimension"].as_u64().unwrap_or(0))
        .unwrap();
    assert!(sent <= 8000, "asked the driver for a {sent}px image");
}

/// Dispatch trims the action; the taint latch does not — a padded
/// observation runs but never latches untrusted.
#[test]
fn padded_observe_action_still_latches_untrusted() {
    let mut reg = registry();
    let dir = session("taintpad");
    let mut c = ctx(&dir);
    let out = reg.call(
        "computer",
        &with(win(), json!({"action": " observe"})),
        &mut c,
    );
    assert!(!out.is_error, "padded observe must dispatch: {}", out.text);
    assert!(
        reg.policy().taint_untrusted(),
        "observation ran ({} bytes) without latching untrusted",
        out.text.len()
    );
}

/// Same mismatch on classification: a padded `navigate` dispatches as a
/// navigation but classifies as InternalWrite, not ExternalComms.
#[test]
fn padded_navigate_is_still_external_comms() {
    let input = json!({"action": "navigate ", "tab": "tab-1", "url": "https://example.com"});
    let dir = session("navpad");
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    run(with(win(), json!({"action": "browser"})), &mut c, &mut st).unwrap();
    run(input.clone(), &mut c, &mut st).unwrap();
    let navigated = calls("schema", Some(&label_of(&dir)))
        .into_iter()
        .any(|(t, _)| t == "browser_navigate");
    assert!(navigated, "padded navigate must reach browser_navigate");
    assert_eq!(classify("computer", &input), Irreversibility::ExternalComms);
}

/// `keys: "+"` and `"cmd++"` split on `+` to `[]` / `["cmd"]` hotkeys the
/// driver refuses (it supports `+` as a key name).
#[test]
fn plus_key_and_cmd_plus_reach_the_driver_in_schema() {
    let dir = session("plus");
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    let a = run(
        with(win(), json!({"action": "key", "keys": "+"})),
        &mut c,
        &mut st,
    );
    let b = run(
        with(win(), json!({"action": "key", "keys": "cmd++"})),
        &mut c,
        &mut st,
    );
    assert!(a.is_ok(), "'+': {a:?}");
    assert!(b.is_ok(), "'cmd++': {b:?}");
}

/// An interrupt raised while a computer batch runs does not stop the batch:
/// every member still drives the real input device.
#[test]
fn interrupt_stops_a_computer_batch_between_members() {
    let _ = fakes();
    let dir = session("intr");
    let label = label_of(&dir);
    let members: Vec<Value> = (0..8)
        .map(|i| with(win(), json!({"action": "click", "x": 10 + i, "y": 10})))
        .collect();
    let provider = Arc::new(Scripted::new(vec![tool_call(
        "c1",
        "computer",
        json!({"action": "batch", "actions": members}),
    )]));
    let cfg = AgentConfig {
        cwd: dir.clone(),
        full_access: true,
        ..Default::default()
    };
    let mut agent = Agent::start(provider, cfg, dir.clone(), "s".into()).unwrap();
    let control = Control::default();
    agent.set_control(control.clone());
    let t0 = Instant::now();
    agent
        .run_turn("click", &mut |e: &Event| {
            if matches!(e.kind, EventKind::ToolCallStart { .. }) {
                control.interrupt();
            }
        })
        .unwrap();
    let clicks = calls("env", Some(&label))
        .into_iter()
        .filter(|(t, _)| t == "click")
        .count();
    assert!(
        clicks <= 1,
        "{clicks} clicks ran after the interrupt ({:?})",
        t0.elapsed()
    );
}

// ============================================================ coverage (held)

#[test]
fn every_observation_latches_untrusted_via_the_registry() {
    let dir = session("taint");
    let obs = [
        json!({"action": "apps"}),
        json!({"action": "windows"}),
        with(win(), json!({"action": "observe"})),
        with(win(), json!({"action": "screenshot"})),
        with(
            win(),
            json!({"action": "zoom", "x1": 0, "y1": 0, "x2": 10, "y2": 10}),
        ),
        with(
            win(),
            json!({"action": "verify", "expect": {"kind": "element_exists", "role": "AXButton"}}),
        ),
        with(win(), json!({"action": "browser"})),
        json!({"action": "batch", "actions": [with(win(), json!({"action": "observe"}))]}),
        json!({"op": "call", "name": "computer", "args": {"action": "apps"}}),
        json!({"action": "OBSERVE", "pid": 11, "window_id": 101}),
    ];
    for (i, input) in obs.iter().enumerate() {
        let mut reg = registry();
        let mut c = ctx(&dir);
        let tool = if input.get("op").is_some() {
            "tools"
        } else {
            "computer"
        };
        let out = reg.call(tool, input, &mut c);
        assert!(!out.is_error, "#{i} {input}: {}", out.text);
        assert!(reg.policy().taint_untrusted(), "#{i} {input} did not latch");
    }
}

#[test]
fn navigate_is_external_comms_and_headless_denies_it() {
    let nav = json!({"action": "navigate", "tab": "tab-1", "url": "https://example.com"});
    assert_eq!(classify("computer", &nav), Irreversibility::ExternalComms);
    let mut reg = registry();
    let dir = session("navdeny");
    let out = reg.call("computer", &nav, &mut ctx(&dir));
    assert!(out.is_error, "{}", out.text);
    assert!(calls("env", Some(&label_of(&dir))).is_empty());
}

/// With both latches armed, headless denies a navigate and a
/// credential-field type (ExternalComms / Identity lanes).
#[test]
fn armed_taint_gates_navigate_and_credential_typing_headless() {
    let mut reg = registry();
    let dir = session("taintact");
    let mut c = ctx(&dir);
    assert!(
        !reg.call(
            "computer",
            &with(win(), json!({"action": "observe"})),
            &mut c
        )
        .is_error
    );
    reg.policy().mark_sensitive("audit");
    assert!(reg.policy().taint_armed());
    let nav = reg.call(
        "computer",
        &json!({"action": "navigate", "tab": "tab-1", "url": "https://attacker.example"}),
        &mut c,
    );
    assert!(nav.is_error, "{}", nav.text);
    let cred = reg.call(
        "computer",
        &with(
            win(),
            json!({"action": "type", "text": "hunter2", "cred_field": true}),
        ),
        &mut c,
    );
    assert!(cred.is_error, "{}", cred.text);
    let sent: Vec<String> = calls("env", Some(&label_of(&dir)))
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    assert!(
        !sent
            .iter()
            .any(|t| t == "browser_navigate" || t == "type_text"),
        "{sent:?}"
    );
}

/// Rule-of-Two: with untrusted + sensitive both latched, `write` and `bash`
/// Ask (headless: deny). A computer `type` — typing into whatever window
/// is focused, e.g. a chat box — runs silently.
#[test]
fn armed_taint_forces_ask_for_a_computer_type_act() {
    let mut reg = registry();
    let dir = session("taintarm");
    let mut c = ctx(&dir);
    assert!(
        !reg.call(
            "computer",
            &with(win(), json!({"action": "observe"})),
            &mut c
        )
        .is_error
    );
    reg.policy().mark_sensitive("audit");
    assert!(reg.policy().taint_armed());
    let w = reg.call(
        "write",
        &json!({"path": dir.join("x.txt").to_str().unwrap(), "content": "s"}),
        &mut c,
    );
    assert!(w.is_error, "control: write must be gated: {}", w.text);
    let out = reg.call(
        "computer",
        &with(
            win(),
            json!({"action": "type", "text": "the secret is ..."}),
        ),
        &mut c,
    );
    let typed = calls("env", Some(&label_of(&dir)))
        .into_iter()
        .any(|(t, _)| t == "type_text");
    assert!(
        out.is_error && !typed,
        "type ran with the exfil triangle armed: {}",
        out.text
    );
}

#[test]
fn stdout_preamble_before_handshake_fails_cleanly_and_retries() {
    let dir = session("pre");
    let (mut c, mut st) = (ctx(&dir), state("preamble"));
    let a = run(json!({"action": "apps"}), &mut c, &mut st);
    let b = run(json!({"action": "apps"}), &mut c, &mut st);
    assert!(a.is_err() && b.is_err(), "{a:?} {b:?}");
    assert_eq!(spawns("preamble"), 2);
}

#[test]
fn wrong_response_id_is_a_transport_error_then_respawn() {
    let dir = session("wid");
    let (mut c, mut st) = (ctx(&dir), state("wrong_id"));
    let a = run(json!({"action": "apps"}), &mut c, &mut st).unwrap_err();
    assert!(a.contains("respawn"), "{a}");
    let _ = run(json!({"action": "apps"}), &mut c, &mut st);
    assert_eq!(spawns("wrong_id"), 2);
}

#[test]
fn malformed_response_line_is_an_error_not_a_panic() {
    let dir = session("garb");
    let (mut c, mut st) = (ctx(&dir), state("garbage"));
    assert!(run(json!({"action": "apps"}), &mut c, &mut st).is_err());
}

#[test]
fn crash_mid_call_drops_and_the_next_call_respawns() {
    let dir = session("crash");
    let (mut c, mut st) = (ctx(&dir), state("crash_call"));
    for _ in 0..3 {
        assert!(run(json!({"action": "apps"}), &mut c, &mut st).is_err());
    }
    assert_eq!(spawns("crash_call"), 3);
}

#[test]
fn respawn_storm_is_one_spawn_per_call_and_fast() {
    let dir = session("storm");
    let (mut c, mut st) = (ctx(&dir), state("crash_init"));
    let t0 = Instant::now();
    for _ in 0..20 {
        assert!(run(json!({"action": "apps"}), &mut c, &mut st).is_err());
    }
    let took = t0.elapsed();
    assert_eq!(spawns("crash_init"), 20);
    eprintln!("AUDIT storm: 20 failing calls, 20 spawns, {took:?}");
    assert!(took < Duration::from_secs(30));
}

#[test]
fn a_hung_driver_times_out_and_is_killed() {
    let dir = session("slow");
    let (mut c, mut st) = (ctx(&dir), state("slow"));
    let t0 = Instant::now();
    let e = run(json!({"action": "apps"}), &mut c, &mut st).unwrap_err();
    let took = t0.elapsed();
    eprintln!("AUDIT timeout: {took:?}: {e}");
    assert!(took < Duration::from_secs(38), "{took:?}");
    assert!(e.contains("respawn"), "{e}");
}

#[test]
fn stderr_flood_does_not_wedge_the_call() {
    let dir = session("flood");
    let (mut c, mut st) = (ctx(&dir), state("stderr_flood"));
    let t0 = Instant::now();
    let r = run(json!({"action": "apps"}), &mut c, &mut st);
    assert!(r.is_ok(), "{r:?}");
    eprintln!("AUDIT stderr flood 16 MiB: {:?}", t0.elapsed());
}

#[test]
fn oversized_text_result_is_bounded_by_the_registry_budget() {
    let dir = session("big");
    let (mut c, mut st) = (ctx(&dir), state("oversized"));
    let raw = run(json!({"action": "apps"}), &mut c, &mut st).unwrap();
    eprintln!(
        "AUDIT oversized: raw tool value {} bytes",
        raw.to_string().len()
    );
}

#[test]
fn huge_tree_observe_is_truncated_to_the_limit() {
    let dir = session("tree");
    let (mut c, mut st) = (ctx(&dir), state("huge_tree"));
    let t0 = Instant::now();
    let v = run(with(win(), json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let text = v.to_string();
    eprintln!(
        "AUDIT 100k-node observe: {:?}, {} bytes out",
        t0.elapsed(),
        text.len()
    );
    assert!(text.len() < 64 * 1024, "{} bytes", text.len());
    let big = run(
        with(win(), json!({"action": "observe", "limit": 1_000_000})),
        &mut c,
        &mut st,
    )
    .unwrap()
    .to_string();
    eprintln!("AUDIT 100k-node observe limit=1e6: {} bytes out", big.len());
    assert!(big.len() < 1024 * 1024);
}

#[test]
fn repeated_observe_and_screenshot_loops_reuse_one_driver() {
    let dir = session("loop");
    let (mut c, mut st) = (ctx(&dir), state("loop"));
    let t0 = Instant::now();
    for _ in 0..50 {
        run(with(win(), json!({"action": "observe"})), &mut c, &mut st).unwrap();
    }
    for _ in 0..30 {
        run(
            with(win(), json!({"action": "screenshot"})),
            &mut c,
            &mut st,
        )
        .unwrap();
    }
    assert_eq!(spawns("loop"), 1);
    let files = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| std::fs::read_dir(e.path()).map(|d| d.count()).unwrap_or(0))
        .sum::<usize>();
    eprintln!(
        "AUDIT loop: 50 observe + 30 screenshot in {:?}; {files} files under session subdirs",
        t0.elapsed()
    );
}

#[test]
fn big_base64_image_is_persisted_not_inlined_in_the_tool_value() {
    let dir = session("bigimg");
    let (mut c, mut st) = (ctx(&dir), state("big_image"));
    let v = run(
        with(win(), json!({"action": "screenshot"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    eprintln!(
        "AUDIT 12 MiB base64: tool value {} bytes",
        v.to_string().len()
    );
    assert!(v.to_string().len() < 64 * 1024);
}

#[test]
fn drop_ends_the_driver_session_with_its_label() {
    let dir = session("cleanup");
    {
        let (mut c, mut st) = (ctx(&dir), state("cleanup"));
        run(json!({"action": "apps"}), &mut c, &mut st).unwrap();
    }
    let ended = calls("cleanup", Some(&label_of(&dir)))
        .into_iter()
        .any(|(t, _)| t == "end_session");
    assert!(ended);
}

#[test]
fn drop_with_a_wedged_end_session_is_bounded() {
    let dir = session("wedge");
    let (mut c, mut st) = (ctx(&dir), state("slow_end"));
    run(json!({"action": "apps"}), &mut c, &mut st).unwrap();
    let t0 = Instant::now();
    drop(st);
    let took = t0.elapsed();
    eprintln!("AUDIT wedged end_session drop: {took:?}");
    assert!(took < Duration::from_secs(5));
}

#[test]
fn helper_fallback_is_not_reachable_while_a_driver_is_configured() {
    let dir = session("fallback");
    let mut st = ComputerState::new(Backends {
        driver: Some(fakes()["fallback"].path.clone()),
        pixel: Some(fakes()["pixel-helper"].path.clone()),
        structured: Some(fakes()["pixel-helper"].path.clone()),
        a11y: Some(fakes()["pixel-helper"].path.clone()),
    });
    let mut c = ctx(&dir);
    for _ in 0..3 {
        assert!(run(
            with(win(), json!({"action": "screenshot"})),
            &mut c,
            &mut st
        )
        .is_err());
    }
    assert!(!fakes()["pixel-helper"].log.exists(), "helper ran");
}

#[test]
fn in_frame_coordinates_and_drag_after_zoom_conform() {
    let dir = session("coordok");
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    run(
        with(win(), json!({"action": "screenshot"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    run(
        with(win(), json!({"action": "click", "x": 0, "y": 799})),
        &mut c,
        &mut st,
    )
    .unwrap();
    run(
        with(
            win(),
            json!({"action": "zoom", "x1": 0, "y1": 0, "x2": 200, "y2": 120}),
        ),
        &mut c,
        &mut st,
    )
    .unwrap();
    run(
        with(
            win(),
            json!({"action": "drag", "x": 1, "y": 2, "to_x": 30, "to_y": 40}),
        ),
        &mut c,
        &mut st,
    )
    .unwrap();
    run(
        with(win(), json!({"action": "click", "x": 3, "y": 4})),
        &mut c,
        &mut st,
    )
    .unwrap();
}

#[test]
fn every_other_action_conforms_to_the_driver_schema() {
    let dir = session("conform");
    let (mut c, mut st) = (ctx(&dir), state("schema"));
    run(with(win(), json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let ok = [
        json!({"action": "apps"}),
        json!({"action": "windows"}),
        json!({"action": "launch", "app": "TextEdit"}),
        with(win(), json!({"action": "observe", "query": "Button"})),
        with(win(), json!({"action": "click", "element": 1})),
        with(
            win(),
            json!({"action": "click", "element": 1, "button": "right"}),
        ),
        with(win(), json!({"action": "click", "element": 1, "count": 2})),
        with(win(), json!({"action": "type", "text": "hi"})),
        with(win(), json!({"action": "type", "text": "hi", "element": 1})),
        with(win(), json!({"action": "key", "keys": "return"})),
        with(win(), json!({"action": "key", "keys": "cmd+s"})),
        with(win(), json!({"action": "set", "element": 1, "value": "v"})),
        with(win(), json!({"action": "scroll", "dy": 3})),
        with(win(), json!({"action": "scroll", "dx": -400})),
        with(
            win(),
            json!({"action": "drag", "x": 1, "y": 2, "to_x": 3, "to_y": 4}),
        ),
        with(win(), json!({"action": "menu", "path": ["File", "Save"]})),
        with(
            win(),
            json!({"action": "verify", "expect": {"kind": "element_exists", "role": "AXButton"}}),
        ),
        with(win(), json!({"action": "browser"})),
        json!({"action": "browser", "tab": "tab-1"}),
        json!({"action": "browser_click", "tab": "tab-1", "ref": "p1:0"}),
        json!({"action": "browser_type", "tab": "tab-1", "ref": "p1:0", "text": "q"}),
        json!({"action": "navigate", "tab": "tab-1", "url": "https://example.com"}),
    ];
    for input in ok {
        let r = run(input.clone(), &mut c, &mut st);
        assert!(r.is_ok(), "{input}: {r:?}");
    }
}

// ------------------------------------------------------------- scripted LLM

struct Scripted(Mutex<VecDeque<Response>>);

impl Scripted {
    fn new(r: Vec<Response>) -> Self {
        Scripted(Mutex::new(r.into()))
    }
}

impl Provider for Scripted {
    fn complete(&self, _req: &Request) -> Result<Response, ProviderError> {
        Ok(self.0.lock().unwrap().pop_front().unwrap_or(Response {
            blocks: vec![Block::Text {
                text: "done".into(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: Usage::default(),
            request_bytes: 0,
            latency_ms: 0,
        }))
    }
    fn name(&self) -> &'static str {
        "audit-scripted"
    }
}

fn tool_call(id: &str, name: &str, input: Value) -> Response {
    Response {
        blocks: vec![Block::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
        }],
        stop_reason: StopReason::ToolUse,
        usage: Usage::default(),
        request_bytes: 0,
        latency_ms: 0,
    }
}
