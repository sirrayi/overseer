//! Computer use wave 2 against a scripted, stateful fake cua-driver: marks,
//! post-act verification, wait_for, schema admission, intake caps, modals,
//! stop_if_changed, the kill switch and secret entry.
//!
//! Like `audit_computer.rs`, the fake is this test binary re-entered through
//! a shell wrapper; it speaks MCP on fd 3 and keeps a small window model in
//! memory for the life of one spawned driver. Its request log stores the
//! sha of every `text`/`value` argument, never the plaintext — what a real
//! driver must do for typed secrets.

use overseer_core::control::Control;
use overseer_core::cred::Broker;
use overseer_core::tools::computer::{self, Backends, ComputerState};
use overseer_core::tools::task::SubagentCtx;
use overseer_core::tools::{Optional, ToolCtx, ToolRegistry};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- fake driver

/// Not a test: the fake driver's entry point when re-entered via a wrapper.
#[test]
fn fake_driver_entry() {
    let Ok(mode) = std::env::var("V2_FAKE_MODE") else {
        return;
    };
    let log = PathBuf::from(std::env::var("V2_FAKE_LOG").unwrap_or_default());
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

fn sha(text: &str) -> String {
    use sha2::{Digest, Sha256};
    format!("sha256:{:x}", Sha256::digest(text.as_bytes()))
}

/// The request with `text`/`value` replaced by their sha.
fn redacted(msg: &Value) -> Value {
    let mut m = msg.clone();
    if let Some(a) = m
        .pointer_mut("/params/arguments")
        .and_then(Value::as_object_mut)
    {
        for k in ["text", "value"] {
            if let Some(s) = a.get(k).and_then(Value::as_str).map(sha) {
                a.insert(k.into(), json!(s));
            }
        }
    }
    m
}

#[derive(Clone)]
struct El {
    role: &'static str,
    label: String,
    value: Option<String>,
    parent: Option<usize>,
    depth: usize,
}

fn el(role: &'static str, label: &str, parent: Option<usize>, depth: usize) -> El {
    El {
        role,
        label: label.into(),
        value: None,
        parent,
        depth,
    }
}

struct Model {
    mode: String,
    els: Vec<El>,
    acts: u32,
    ax_reads: u32,
    verify_polls: u32,
}

impl Model {
    fn new(mode: &str) -> Self {
        let els = match mode {
            "modal" => vec![
                el("AXWindow", "Doc", None, 0),
                el("AXButton", "Behind", Some(0), 1),
                el("AXSheet", "Save changes?", Some(0), 1),
                el("AXButton", "Save", Some(2), 2),
                el("AXButton", "Cancel", Some(2), 2),
            ],
            "huge" => (0..100_000)
                .map(|i| {
                    if i == 0 {
                        el("AXWindow", "Doc", None, 0)
                    } else {
                        el("AXButton", &format!("Button {i}"), Some(0), 1)
                    }
                })
                .collect(),
            _ => {
                let mut name = el("AXTextField", "Name", Some(0), 1);
                name.value = Some("hi".into());
                vec![
                    el("AXWindow", "Doc", None, 0),
                    name,
                    el("AXButton", "Go", Some(0), 1),
                ]
            }
        };
        Model {
            mode: mode.into(),
            els,
            acts: 0,
            ax_reads: 0,
            verify_polls: 0,
        }
    }

    fn tree(&self) -> Value {
        let els: Vec<Value> = self
            .els
            .iter()
            .enumerate()
            .map(|(i, e)| {
                json!({"element_index": i, "element_token": format!("tok-{}", e.label),
                       "role": e.role, "label": e.label, "value": e.value,
                       "actions": ["AXPress"], "depth": e.depth, "parent_index": e.parent})
            })
            .collect();
        let n = els.len();
        json!({"content": [{"type": "text", "text": "tree"}],
               "structuredContent": {"snapshot_id": format!("snap-{}", self.ax_reads),
                                     "elements": els, "element_count": n}})
    }
}

fn obj(required: &[&str], props: &[&str]) -> Value {
    let p: Map<String, Value> = props.iter().map(|k| (k.to_string(), json!({}))).collect();
    json!({"type": "object", "required": required, "properties": p})
}

/// What `tools/list` advertises per mode (empty = no admission).
fn advertised(mode: &str) -> Vec<(&'static str, Value)> {
    let gws = obj(
        &["pid", "window_id"],
        &[
            "pid",
            "window_id",
            "include_screenshot",
            "include_accessibility_tree",
            "max_dimension",
            "max_elements",
            "max_depth",
            "query",
        ],
    );
    let click = obj(
        &[],
        &[
            "pid",
            "window_id",
            "x",
            "y",
            "element_token",
            "button",
            "count",
        ],
    );
    match mode {
        "schema" | "skew" => {
            let mut verify = obj(
                &["pid", "window_id", "expect"],
                &["pid", "window_id", "expect"],
            );
            verify["properties"]["expect"] = json!({"type": "array", "minItems": 1});
            let scroll_amount = if mode == "skew" { "clicks" } else { "amount" };
            vec![
                ("get_window_state", gws),
                (
                    "zoom",
                    obj(
                        &["window_id", "x1", "y1", "x2", "y2"],
                        &["pid", "window_id", "x1", "y1", "x2", "y2"],
                    ),
                ),
                ("click", click),
                (
                    "type_text",
                    obj(
                        &["text", "pid", "window_id"],
                        &["text", "pid", "window_id", "element_token"],
                    ),
                ),
                ("verify_state", verify),
                (
                    "scroll",
                    obj(
                        &["direction"],
                        &[
                            "direction",
                            scroll_amount,
                            "pid",
                            "window_id",
                            "x",
                            "y",
                            "element_token",
                        ],
                    ),
                ),
            ]
        }
        "axwait" => vec![("get_window_state", gws), ("click", click)],
        _ => Vec::new(),
    }
}

fn violation(mode: &str, tool: &str, args: &Value) -> Option<String> {
    let schemas = advertised(mode);
    let (_, s) = schemas.iter().find(|(n, _)| *n == tool)?;
    let a = args.as_object()?;
    let props = s["properties"].as_object()?;
    for k in a.keys() {
        if k != "session" && !props.contains_key(k) {
            return Some(format!("{tool}: unknown argument {k}"));
        }
    }
    for r in s["required"].as_array()?.iter().filter_map(Value::as_str) {
        if !a.contains_key(r) {
            return Some(format!("{tool}: missing {r}"));
        }
    }
    None
}

fn refusal(code: &str, why: &str) -> Value {
    json!({"isError": true,
           "content": [{"type": "text", "text": format!("refused ({code}): {why}")}],
           "structuredContent": {"status": "refused", "refusal": {"code": code, "message": why}}})
}

const ACT_TOOLS: &[&str] = &[
    "click",
    "type_text",
    "press_key",
    "hotkey",
    "set_value",
    "scroll",
];

fn image(data: String, w: u64) -> Value {
    json!({"content": [{"type": "text", "text": "captured"},
                       {"type": "image", "data": data, "mimeType": "image/png"}],
           "structuredContent": {"screenshot_width": w, "screenshot_height": w * 5 / 8,
                                 "width": w, "height": w * 5 / 8,
                                 "window_bounds": {"width": w, "height": w * 5 / 8}}})
}

fn handle(m: &mut Model, name: &str, args: &Value) -> Value {
    if let Some(why) = violation(&m.mode, name, args) {
        return refusal("invalid_arguments", &why);
    }
    let mode = m.mode.clone();
    if ACT_TOOLS.contains(&name) {
        m.acts += 1;
        if mode == "killsw" && name == "click" {
            return refusal("kill_switch", "the user pressed Escape");
        }
        let token = args["element_token"].as_str().unwrap_or("");
        match mode.as_str() {
            "marks" if name == "click" && token == "tok-Go" => {
                m.els.retain(|e| e.label != "Go");
                m.els.push(el("AXButton", "Done", Some(0), 1));
            }
            "change" if name == "click" => {
                for k in 0..60 {
                    m.els.push(el(
                        "AXButton",
                        &format!("Added button number {k} with a fairly long label"),
                        Some(0),
                        1,
                    ));
                }
            }
            "batch2" if m.acts == 2 => m.els.push(el("AXStaticText", "Changed", Some(0), 1)),
            "secret" if name == "type_text" || name == "set_value" => {
                let v = args["text"]
                    .as_str()
                    .or(args["value"].as_str())
                    .unwrap_or("")
                    .to_string();
                m.els[1].value = Some(v.clone());
                return json!({"content": [{"type": "text", "text": format!("typed {v}")}]});
            }
            _ => {}
        }
        return json!({"content": [{"type": "text", "text": format!("done {name}")}]});
    }
    match name {
        "get_window_state" if args["include_accessibility_tree"] == json!(false) => {
            let max = args["max_dimension"].as_u64().unwrap_or(1280);
            let data = if mode == "big" {
                "A".repeat(3 << 20)
            } else {
                "iVBORw0KGgo=".into()
            };
            image(data, max)
        }
        "get_window_state" => {
            m.ax_reads += 1;
            if mode == "axwait" && m.ax_reads >= 3 && !m.els.iter().any(|e| e.label == "Ready") {
                m.els.push(el("AXStaticText", "Ready", Some(0), 1));
            }
            m.tree()
        }
        "zoom" => image("iVBORw0KGgo=".into(), 500),
        "verify_state" => {
            m.verify_polls += 1;
            let ok = mode == "verify3" && m.verify_polls >= 3;
            json!({"content": [{"type": "text", "text": "verified"}],
                   "structuredContent": {"status": if ok { "satisfied" } else { "unsatisfied" },
                                         "predicates": []}})
        }
        _ => json!({"content": [{"type": "text", "text": format!("done {name}")}]}),
    }
}

fn serve(mode: &str, log: &Path) {
    use std::os::fd::FromRawFd;
    // SAFETY: the wrapper dup'd the MCP stdout pipe onto fd 3.
    let mut out = unsafe { std::fs::File::from_raw_fd(3) };
    log_to(log, &format!("spawn {mode}"));
    let mut model = Model::new(mode);
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        log_to(log, &format!("req {}", redacted(&msg)));
        let id = msg.get("id").cloned().unwrap_or(Value::Null);
        let result = match msg["method"].as_str().unwrap_or("") {
            "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {"tools": {}},
                                   "serverInfo": {"name": "v2-fake-cua-driver", "version": "0.34.0"}}),
            m if m.starts_with("notifications/") => continue,
            "tools/list" => json!({"tools": advertised(mode)
                .into_iter()
                .map(|(n, s)| json!({"name": n, "description": n, "inputSchema": s}))
                .collect::<Vec<_>>()}),
            "tools/call" => {
                let name = msg["params"]["name"].as_str().unwrap_or("").to_string();
                handle(&mut model, &name, &msg["params"]["arguments"])
            }
            _ => continue,
        };
        // One write: Display straight into the unbuffered fd would be a
        // syscall per token, and a 100K-node line would time the fake.
        let mut line = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        line.push('\n');
        let _ = out.write_all(line.as_bytes());
        let _ = out.flush();
    }
}

// ------------------------------------------------------------- test harness

/// One wrapper per test (own log), all written before any spawn (ETXTBSY).
const FAKES: &[(&str, &str)] = &[
    ("marks", "marks"),
    ("unobserved", "normal"),
    ("change", "change"),
    ("nobase", "normal"),
    ("batchpost", "normal"),
    ("verify3", "verify3"),
    ("never", "never"),
    ("intr", "never"),
    ("axwait", "axwait"),
    ("schema", "schema"),
    ("required", "schema"),
    ("skew", "skew"),
    ("huge", "huge"),
    ("unchanged", "normal"),
    ("big", "big"),
    ("modal", "modal"),
    ("batch2", "batch2"),
    ("batch2_off", "batch2"),
    ("kill", "normal"),
    ("killsw", "killsw"),
    ("secret", "secret"),
    ("latch", "normal"),
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
                "#!/bin/sh\nexport V2_FAKE_MODE='{mode}'\nexport V2_FAKE_LOG='{}'\n\
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
        m
    })
}

fn tmp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!(
        "overseer-computer-v2-{tag}-{}",
        uuid::Uuid::now_v7()
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ctx(dir: &Path) -> ToolCtx<'static> {
    ToolCtx {
        cwd: dir.to_path_buf(),
        session_dir: dir.to_path_buf(),
        spill_seq: 0,
        provider: None,
        agent_config: None,
        subagents: SubagentCtx {
            seq: 0,
            spend: None,
        },
        checkpoint: None,
        sandbox: false,
        broker: None,
    }
}

/// A fresh session dir, its ctx, and a state driving the `tag` fake.
fn setup(tag: &str) -> (ToolCtx<'static>, ComputerState) {
    let st = ComputerState::new(Backends {
        driver: Some(fakes()[tag].path.clone()),
        ..Default::default()
    });
    (ctx(&tmp(tag)), st)
}

fn run(input: Value, c: &mut ToolCtx, st: &mut ComputerState) -> Result<Value, String> {
    computer::run_with(&input, c, st)
}

fn act(extra: Value) -> Value {
    let mut b = json!({"pid": 11, "window_id": 101});
    b.as_object_mut()
        .unwrap()
        .extend(extra.as_object().unwrap().clone());
    b
}

/// `(tool, arguments)` of every `tools/call` the `tag` fake received.
fn calls(tag: &str) -> Vec<(String, Value)> {
    std::fs::read_to_string(&fakes()[tag].log)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| l.strip_prefix("req "))
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|m| m["method"] == "tools/call")
        .map(|m| {
            (
                m["params"]["name"].as_str().unwrap_or("").to_string(),
                m["params"]["arguments"].clone(),
            )
        })
        .collect()
}

fn named(tag: &str, tool: &str) -> Vec<Value> {
    calls(tag)
        .into_iter()
        .filter(|(t, _)| t == tool)
        .map(|(_, a)| a)
        .collect()
}

fn ax_reads(tag: &str) -> usize {
    named(tag, "get_window_state")
        .iter()
        .filter(|a| a["include_screenshot"] == json!(false))
        .count()
}

// ========================================================== P2 element marks

#[test]
fn marks_resolve_to_tokens_and_stale_or_unknown_marks_never_dispatch() {
    let (mut c, mut st) = setup("marks");
    let v = run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let text = v["text"].as_str().unwrap();
    assert!(
        text.contains("[e2] AXTextField \"Name\" = \"hi\""),
        "{text}"
    );
    assert!(text.contains("[e3] AXButton \"Go\""), "{text}");

    // A mark resolves to its driver token from the latest snapshot.
    run(
        act(json!({"action": "type", "element": "e2", "text": "x"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(named("marks", "type_text")[0]["element_token"], "tok-Name");
    // Clicking Go replaces it with Done in the fake's window.
    run(
        act(json!({"action": "click", "element": "e3"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(named("marks", "click")[0]["element_token"], "tok-Go");
    let v = run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let text = v["text"].as_str().unwrap();
    assert!(text.contains("[e4] AXButton \"Done\""), "{text}");
    assert!(
        text.contains("[e2] AXTextField"),
        "unchanged elements keep their mark: {text}"
    );

    let clicks = named("marks", "click").len();
    let stale = run(
        act(json!({"action": "click", "element": "e3"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(stale.contains("e3 is stale"), "{stale}");
    let unknown = run(
        act(json!({"action": "click", "element": "e99"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(unknown.contains("e99 is unknown"), "{unknown}");
    let junk = run(
        act(json!({"action": "click", "element": "go"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(junk.contains("mark like"), "{junk}");
    assert_eq!(
        named("marks", "click").len(),
        clicks,
        "a refused mark dispatches nothing"
    );

    run(
        act(json!({"action": "click", "element": "e4"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    for a in named("marks", "click")
        .iter()
        .chain(named("marks", "type_text").iter())
    {
        assert!(
            a["element_token"].is_string(),
            "mark act sent no token: {a}"
        );
        assert!(a.get("x").is_none(), "mark act sent coords: {a}");
    }
}

#[test]
fn a_mark_for_an_unobserved_window_is_refused_locally() {
    let (mut c, mut st) = setup("unobserved");
    let err = run(
        act(json!({"action": "click", "element": "e1"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("has not been observed"), "{err}");
    assert!(named("unobserved", "click").is_empty());
}

// ======================================================= P3 verify after act

#[test]
fn post_act_read_reports_changed_with_a_capped_diff() {
    let (mut c, mut st) = setup("change");
    run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let v = run(
        act(json!({"action": "click", "element": "e3"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(v["changed"], true, "{v}");
    assert_ne!(v["pre"], v["post"], "{v}");
    assert!(v["post"].as_str().unwrap().starts_with("sha256:"), "{v}");
    let diff = v["diff"].as_str().unwrap();
    assert!(diff.len() <= 1024, "diff is {} bytes", diff.len());
    assert!(
        diff.starts_with("+[e4] AXButton \"Added button number 0"),
        "{diff}"
    );
    assert!(
        diff.contains("more"),
        "the cap says how much was cut: {diff}"
    );

    // No change: a key press leaves the window alone.
    let v = run(
        act(json!({"action": "key", "keys": "return"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(v["changed"], false, "{v}");
    assert_eq!(v["pre"], v["post"], "{v}");
    assert!(v.get("diff").is_none(), "{v}");
}

#[test]
fn an_unobserved_window_gets_the_post_sha_but_no_screen_text() {
    let (mut c, mut st) = setup("nobase");
    let v = run(
        act(json!({"action": "key", "keys": "return"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert!(v["post"].as_str().unwrap().starts_with("sha256:"), "{v}");
    assert!(v["changed"].is_null(), "{v}");
    assert!(v.get("diff").is_none(), "{v}");
    assert_eq!(ax_reads("nobase"), 1);
}

#[test]
fn a_batch_takes_one_post_read_after_the_last_member() {
    let (mut c, mut st) = setup("batchpost");
    run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let before = ax_reads("batchpost");
    let members: Vec<Value> = (0..3)
        .map(|_| act(json!({"action": "key", "keys": "return"})))
        .collect();
    let v = run(
        json!({"action": "batch", "actions": members}),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(v["count"], 3);
    assert_eq!(ax_reads("batchpost") - before, 1, "one post read per batch");
    assert_eq!(v["changed"], false, "{v}");
    for r in v["results"].as_array().unwrap() {
        assert!(r["post"].is_null(), "members skip the post read: {r}");
    }
}

/// The registry latches untrusted for observations through perm.rs's own
/// list; an act's post read is not in it, and perm.rs is outside this
/// slice. The diff's screen text only extends an observe that already
/// latched (see `an_unobserved_window_gets_the_post_sha_but_no_screen_text`).
#[test]
#[ignore = "needs perm.rs to treat a computer act's post read as screenshot context"]
fn an_acts_post_read_latches_untrusted_on_its_own() {
    let _ = fakes();
    std::env::set_var("OVERSEER_COMPUTER_DRIVER", &fakes()["latch"].path);
    let mut reg = ToolRegistry::core_with(overseer_core::perm::Policy::allow_all(), Optional::ALL);
    let mut c = ctx(&tmp("latch"));
    let out = reg.call(
        "computer",
        &act(json!({"action": "key", "keys": "return"})),
        &mut c,
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(reg.policy().taint_untrusted());
}

// ================================================================ P4 wait_for

#[test]
fn wait_for_is_met_on_the_third_poll_with_backoff() {
    let (mut c, mut st) = setup("verify3");
    let expect = json!([{"kind": "element_exists", "role": "AXButton", "label": "Done"}]);
    let v = run(
        act(json!({"action": "wait_for", "expect": expect, "timeout_ms": 5000})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(v["met"], true, "{v}");
    assert_eq!(v["polls"], 3, "{v}");
    // 100 ms + 200 ms of backoff before the third poll.
    assert!(v["elapsed_ms"].as_u64().unwrap() >= 300, "{v}");
    assert_eq!(named("verify3", "verify_state").len(), 3);
}

#[test]
fn wait_for_timeout_is_bounded_by_one_poll() {
    let (mut c, mut st) = setup("never");
    let t0 = Instant::now();
    let v = run(
        act(json!({"action": "wait_for", "expect": {"role": "AXButton"}, "timeout_ms": 700})),
        &mut c,
        &mut st,
    )
    .unwrap();
    let wall = t0.elapsed();
    assert_eq!(v["met"], false, "{v}");
    assert_eq!(v["timeout"], true, "{v}");
    let elapsed = v["elapsed_ms"].as_u64().unwrap();
    assert!((700..700 + 250).contains(&elapsed), "{v}");
    assert!(wall < Duration::from_millis(700 + 1000), "{wall:?}");
}

#[test]
fn an_interrupt_stops_wait_for_between_polls() {
    let (mut c, mut st) = setup("intr");
    let control = Control::default();
    st.set_control(control.clone());
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(400));
        control.interrupt();
    });
    let t0 = Instant::now();
    let v = run(
        act(json!({"action": "wait_for", "expect": {"role": "AXButton"}, "timeout_ms": 10_000})),
        &mut c,
        &mut st,
    )
    .unwrap();
    t.join().unwrap();
    assert_eq!(v["interrupted"], true, "{v}");
    assert_eq!(v["met"], false, "{v}");
    assert!(
        t0.elapsed() < Duration::from_millis(1500),
        "{:?}",
        t0.elapsed()
    );
}

#[test]
fn wait_for_falls_back_to_ax_state_without_verify_state() {
    let (mut c, mut st) = setup("axwait");
    let v = run(
        act(json!({"action": "wait_for", "expect": {"label": "Ready"}, "timeout_ms": 5000})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(v["met"], true, "{v}");
    assert_eq!(v["polls"], 3, "{v}");
    assert!(named("axwait", "verify_state").is_empty());
}

#[test]
fn wait_for_is_an_observe_class_action() {
    assert!(computer::OBSERVE_ACTIONS.contains(&"wait_for"));
}

// ===================================================== P8 schema admission

#[test]
fn a_click_after_zoom_drops_from_zoom_the_driver_does_not_list() {
    let (mut c, mut st) = setup("schema");
    run(act(json!({"action": "screenshot"})), &mut c, &mut st).unwrap();
    run(
        act(json!({"action": "zoom", "x1": 0, "y1": 0, "x2": 100, "y2": 60})),
        &mut c,
        &mut st,
    )
    .unwrap();
    let v = run(
        act(json!({"action": "click", "x": 30, "y": 15})),
        &mut c,
        &mut st,
    )
    .unwrap();
    let click = &named("schema", "click")[0];
    assert!(click.get("from_zoom").is_none(), "{click}");
    assert!(
        v["debug"].as_str().unwrap().contains("click.from_zoom"),
        "{v}"
    );
}

#[test]
fn a_missing_required_or_short_array_arg_is_refused_locally() {
    let (mut c, mut st) = setup("required");
    // The advertised type_text requires window_id; `type` alone needs pid.
    let err = run(
        json!({"action": "type", "pid": 11, "text": "hi"}),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("requires 'window_id'"), "{err}");
    assert!(named("required", "type_text").is_empty());
    // verify_state advertises expect minItems 1.
    let err = run(
        act(json!({"action": "wait_for", "expect": [], "timeout_ms": 100})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("at least 1"), "{err}");
    assert!(named("required", "verify_state").is_empty());
}

#[test]
fn driver_version_skew_drops_a_renamed_optional_arg() {
    let (mut c, mut st) = setup("skew");
    let v = run(act(json!({"action": "scroll", "dy": 3})), &mut c, &mut st).unwrap();
    let scroll = &named("skew", "scroll")[0];
    assert!(scroll.get("amount").is_none(), "{scroll}");
    assert_eq!(scroll["direction"], "down");
    assert!(
        v["debug"].as_str().unwrap().contains("scroll.amount"),
        "{v}"
    );
}

// ============================================================ P7 intake caps

#[test]
fn a_100k_node_observe_is_fast_and_bounded() {
    let (mut c, mut st) = setup("huge");
    // Warm the spawn so only the observe is timed.
    run(json!({"action": "apps"}), &mut c, &mut st).unwrap();
    let t0 = Instant::now();
    let v = run(
        act(json!({"action": "observe", "depth": 3})),
        &mut c,
        &mut st,
    )
    .unwrap();
    let took = t0.elapsed();
    eprintln!("100k-node observe: {took:?}");
    let a = &named("huge", "get_window_state")[0];
    assert_eq!(a["max_elements"], 150, "{a}");
    assert_eq!(a["max_depth"], 3, "{a}");
    let text = v["text"].as_str().unwrap();
    assert!(text.contains("more"), "says the tree was cut");
    assert!(text.len() < 16 * 1024, "{} bytes", text.len());
    if !cfg!(debug_assertions) {
        assert!(took < Duration::from_secs(1), "{took:?}");
    }
}

#[test]
fn an_identical_screenshot_is_unchanged_with_no_image_block() {
    let (mut c, mut st) = setup("unchanged");
    let first = computer::run(&act(json!({"action": "screenshot"})), &mut c, &mut st);
    assert!(!first.is_error, "{}", first.text);
    assert!(computer::image_block(&first.text).is_some());
    let again = computer::run(&act(json!({"action": "screenshot"})), &mut c, &mut st);
    let v: Value = serde_json::from_str(&again.text).unwrap();
    assert_eq!(v["unchanged"], true, "{v}");
    assert!(
        computer::image_block(&again.text).is_none(),
        "no second copy"
    );
    // The provider limit already in code (8000 px) clamps max_dimension.
    run(
        json!({"action": "screenshot", "pid": 11, "window_id": 102, "max": 99_999}),
        &mut c,
        &mut st,
    )
    .unwrap();
    let last = named("unchanged", "get_window_state").pop().unwrap();
    assert_eq!(last["max_dimension"], 8000, "{last}");
    // A new turn (new Control) forgets what the model saw.
    st.set_control(Control::default());
    let v = run(act(json!({"action": "screenshot"})), &mut c, &mut st).unwrap();
    assert!(v.get("unchanged").is_none(), "{v}");
}

#[test]
fn the_per_turn_image_budget_refuses_and_points_at_observe() {
    let (mut c, mut st) = setup("big");
    for w in 101..104 {
        run(
            json!({"action": "screenshot", "pid": 11, "window_id": w}),
            &mut c,
            &mut st,
        )
        .unwrap();
    }
    let err = run(
        json!({"action": "screenshot", "pid": 11, "window_id": 104}),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(
        err.contains("image budget") && err.contains("observe"),
        "{err}"
    );
    assert_eq!(
        named("big", "get_window_state").len(),
        3,
        "refused before dispatch"
    );
    st.set_control(Control::default());
    run(
        json!({"action": "screenshot", "pid": 11, "window_id": 104}),
        &mut c,
        &mut st,
    )
    .unwrap();
}

// ================================================================= P9 modals

#[test]
fn a_sheet_is_lifted_and_blocks_parent_acts_but_not_its_buttons() {
    let (mut c, mut st) = setup("modal");
    let v = run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    assert_eq!(v["modal"]["role"], "AXSheet", "{v}");
    assert_eq!(v["modal"]["title"], "Save changes?", "{v}");
    assert_eq!(
        v["modal"]["buttons"],
        json!([{"mark": "e4", "label": "Save"}, {"mark": "e5", "label": "Cancel"}])
    );
    let err = run(
        act(json!({"action": "click", "element": "e2"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("dismiss the modal first"), "{err}");
    assert!(err.contains("e4 \"Save\""), "{err}");
    assert!(named("modal", "click").is_empty());
    run(
        act(json!({"action": "click", "element": "e4"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(named("modal", "click")[0]["element_token"], "tok-Save");
}

// ======================================================= P10 stop_if_changed

#[test]
fn stop_if_changed_stops_before_the_member_after_a_change() {
    let (mut c, mut st) = setup("batch2");
    let members: Vec<Value> = (0..4)
        .map(|_| act(json!({"action": "key", "keys": "return"})))
        .collect();
    let v = run(
        json!({"action": "batch", "stop_if_changed": true, "actions": members}),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(v["count"], 2, "{v}");
    let note = v["stopped"].as_str().unwrap();
    assert!(
        note.contains("after actions[1]") && note.contains("stopped at actions[2]"),
        "{note}"
    );
    assert_eq!(named("batch2", "press_key").len(), 2);

    // Default off: the same change does not stop the batch.
    let (mut c, mut st) = setup("batch2_off");
    let members: Vec<Value> = (0..4)
        .map(|_| act(json!({"action": "key", "keys": "return"})))
        .collect();
    let v = run(
        json!({"action": "batch", "actions": members}),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(v["count"], 4, "{v}");
}

// ============================================================= P5 kill switch

#[test]
fn a_kill_notification_interrupts_and_refuses_acts_until_the_next_turn() {
    let (mut c, mut st) = setup("kill");
    let control = Control::default();
    st.set_control(control.clone());
    run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    assert!(!st.driver_notification(&json!({"jsonrpc": "2.0", "method": "notifications/progress"})));
    assert!(
        st.driver_notification(&json!({"jsonrpc": "2.0", "method": "notifications/kill_switch"}))
    );
    assert!(control.interrupted());
    assert!(st.kill_latched());
    let err = run(
        act(json!({"action": "key", "keys": "return"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("kill switch"), "{err}");
    assert!(named("kill", "press_key").is_empty());
    // Observation is still allowed.
    run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    // The next user turn mints a new Control.
    st.set_control(Control::default());
    run(
        act(json!({"action": "key", "keys": "return"})),
        &mut c,
        &mut st,
    )
    .unwrap();
    assert_eq!(named("kill", "press_key").len(), 1);
}

#[test]
fn a_kill_switch_refusal_from_the_driver_latches_too() {
    let (mut c, mut st) = setup("killsw");
    let control = Control::default();
    st.set_control(control.clone());
    let err = run(
        act(json!({"action": "click", "x": 1, "y": 1})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("kill switch"), "{err}");
    assert!(control.interrupted() && st.kill_latched());
    run(
        act(json!({"action": "key", "keys": "return"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(named("killsw", "press_key").is_empty());
}

// =========================================================== P1b secret entry

#[test]
fn a_secret_is_typed_by_name_and_never_echoed() {
    const REAL: &str = "hunter2-real-secret";
    let (mut c, mut st) = setup("secret");
    let mut broker = Broker::new();
    broker.issue_capability("bank_pin", "BANK_PIN", REAL, vec![], vec![], None);
    c.broker = Some(broker);
    run(act(json!({"action": "observe"})), &mut c, &mut st).unwrap();
    let out = computer::run(
        &act(json!({"action": "type", "element": "e2", "secret": "bank_pin"})),
        &mut c,
        &mut st,
    );
    assert!(!out.is_error, "{}", out.text);
    assert!(!out.text.contains(REAL), "{}", out.text);
    let v: Value = serde_json::from_str(&out.text).unwrap();
    assert_eq!(v["secret"]["len"], REAL.len(), "{v}");
    assert_eq!(
        v["secret"]["sha8"],
        sha(REAL)["sha256:".len()..][..8],
        "{v}"
    );
    assert!(v["detail"].as_str().unwrap().contains("[secret]"), "{v}");
    assert!(v["diff"].as_str().unwrap().contains("[secret]"), "{v}");
    let ev = format!("{:?}", computer::audit_event(&out.text));
    assert!(!ev.contains(REAL), "{ev}");
    // set_value too.
    let out = computer::run(
        &act(json!({"action": "set", "element": "e2", "secret": "bank_pin"})),
        &mut c,
        &mut st,
    );
    assert!(!out.is_error && !out.text.contains(REAL), "{}", out.text);
    // The driver got the real value; its log only ever holds the sha.
    assert_eq!(named("secret", "type_text")[0]["text"], sha(REAL));
    assert_eq!(named("secret", "set_value")[0]["value"], sha(REAL));
    let log = std::fs::read_to_string(&fakes()["secret"].log).unwrap();
    assert!(!log.contains(REAL));
    // Unknown names and both-at-once are refused before dispatch.
    let err = run(
        act(json!({"action": "type", "secret": "nope"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("no credential named 'nope'"), "{err}");
    let err = run(
        act(json!({"action": "type", "text": "x", "secret": "bank_pin"})),
        &mut c,
        &mut st,
    )
    .unwrap_err();
    assert!(err.contains("not both"), "{err}");
    assert_eq!(named("secret", "type_text").len(), 1);
}
