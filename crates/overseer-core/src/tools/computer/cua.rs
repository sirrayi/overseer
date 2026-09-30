//! cua-driver backend for the `computer` tool (S5, D2/D3/D4/D6): one
//! long-lived `cua-driver mcp` stdio child per registry, spawned lazily on
//! first use and held in [`super::ComputerState`] the same way `mcp_tool`
//! holds servers. A transport failure drops the client; the next call
//! respawns it. The child env is `PATH` + `HOME` only (spawn_with_env) and
//! every `tools/call` carries `session: "ovs-<8 chars>"` so the driver
//! scopes snapshots, browser targets and refs to this overseer session.
//!
//! ## Response shapes this file relies on
//!
//! Verified live against cua-driver 0.26.1 on Linux/X11 (this box) unless
//! marked *[dump]* (owner's macOS dump) or *[schema]* (tools/list input
//! schema on this box). Parsing stays defensive regardless.
//!
//! - `list_apps` → `structuredContent.apps[]` = `{pid, name, running,
//!   active, launch_path, bundle_id, kind, last_used, windows[]}`. Kernel
//!   and background processes are listed too — `apps` keeps `running`.
//! - `list_windows` → `structuredContent.windows[]` = `{window_id, pid,
//!   app_name, title, x, y, width, height, bounds{…}, z_index,
//!   is_on_screen}`.
//! - `get_window_state` with `include_screenshot:false` →
//!   `structuredContent.elements[]` = `{element_index, element_token
//!   ("s00000001:0"), role, label, value, actions[], depth, parent_index}`
//!   plus `snapshot_id`, `element_count`, `returned_element_count`,
//!   `total_element_count`, `elements_complete`, `degraded`,
//!   `degraded_reason`, `tree_markdown` (never rendered — D4),
//!   `window_bounds{…}`, `window_title`, `app_name`, `pid`.
//! - `get_window_state` with `include_accessibility_tree:false` →
//!   `content[0]` = `{type:"image", data:b64, mimeType:"image/png"}` and
//!   `structuredContent` = `{screenshot_width, screenshot_height,
//!   screenshot_mime_type, window_bounds{height,width,x,y}, window_id,
//!   window_title, app_name, pid}`. `screenshot_*` is the frame the model
//!   sees; `window_bounds` is the native window — the coordinate scale.
//! - `zoom` → `content[0]` image `{data, mimeType:"image/jpeg"}`;
//!   `structuredContent` = `{format, height, width, mime_type}`.
//! - acts (`click`, `right_click`, `double_click`, `type_text`,
//!   `press_key`, `hotkey`, `set_value`, `scroll`, `drag`, `invoke_menu`,
//!   `launch_app`) → `content[0].text` = "✅ …"; `structuredContent` =
//!   `{delivery:{mode}, effect, route}`.
//! - refusals → `isError:true` + `structuredContent.refusal = {code,
//!   message, escalation?}`. Codes observed live: `snapshot_id_required`
//!   ("bare element_index is not accepted"), `stale_element_token`,
//!   `browser_route_unavailable`, `background_unavailable` (with
//!   `escalation.recommended:"foreground"`). Plain argument errors:
//!   `structuredContent = {code:"invalid_arguments", detail, tool}`.
//! - `verify_state` → `structuredContent = {status: "satisfied"|
//!   "unsatisfied"|"unknown", predicates[]: {index, status, observed_json,
//!   unknown_reason}, samples, stable, elapsed_ms}`. `expect` is a *list*
//!   of predicate objects (a bare object is an invalid_arguments error).
//! - `check_permissions` → `structuredContent` is a platform-keyed bool
//!   map (Linux: `{atspi, x11, wayland, xsend_event, …}`; *[dump]* macOS:
//!   Accessibility / Screen Recording).
//! - `get_browser_state`: bind mode `{pid, window_id}` → *[dump]*
//!   `target_id` + `tab_id` (session-scoped); snapshot mode `{target_id,
//!   tab_id, snapshot_format:"semantic_v2", query?}` → *[dump]* refs
//!   `p<snap>:<idx>`. The bind result's nesting is searched recursively —
//!   see `shape::target_and_tab`.
//! - browser acts: `browser_click {target_id, tab_id, ref|x,y}`,
//!   `browser_type {target_id, tab_id, ref, text}`, `browser_navigate
//!   {target_id, tab_id, url}` — url limited to http/https/about.
//!
//! Deviations from the owner's dump found live on 0.26.1: a bare
//! `element_index` is REFUSED (element_token or snapshot_id+element_index
//! required — snapshots are remembered per window to satisfy it),
//! `verify_state.expect` is a sequence not a map, `hotkey.keys` is a list
//! of key names (not "ctrl+l"), `invoke_menu` requires `window_id`, and
//! `scroll` takes `direction` + `amount` (1–50) not raw deltas.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use serde_json::{json, Map, Value};

use super::super::ToolCtx;
use super::{action_of, cred_field, digest_short, read_obs, shape, write_owner_only, ObsState};

use crate::mcp::{StdioClient, CALL_TIMEOUT};

/// `end_session` gets a short leash: it is best-effort teardown, and a wedged
/// driver must not hold the registry's drop for the full call timeout.
const END_SESSION_TIMEOUT: Duration = Duration::from_secs(2);

/// The one coordinate rule for driver acts (F2): the model sends x/y in
/// the frame of the image it was shown (`sent`); `driver` is the space
/// the driver's x/y acts expect; `origin` is where sent's (0,0) sits in
/// driver space (nonzero only for region views).
///
/// Default: `driver == sent` — NO local scaling. That trusts
/// cua-driver's own contract (click doc, verified on 0.26.1
/// tools/list): x/y are "window-local screenshot pixels" — the space
/// `get_window_state` itself returns, i.e. the delivered image, not the
/// native framebuffer.
// DEFERRED(owner): confirm on macOS with computer_live; flip to
// window_bounds×scale if clicks land off.
struct CoordFrame {
    sent: (u32, u32),
    driver: (u32, u32),
    origin: (f64, f64),
}

impl CoordFrame {
    fn sent_only(w: u32, h: u32) -> Self {
        CoordFrame {
            sent: (w, h),
            driver: (w, h),
            origin: (0.0, 0.0),
        }
    }

    /// `origin + v × driver/sent`, clamped into the driver frame; a
    /// zero-sided frame passes the value through rounded.
    fn map(&self, x: f64, y: f64) -> (i64, i64) {
        let conv = |v: f64, sent: u32, drv: u32, o: f64| -> i64 {
            if sent == 0 || drv == 0 {
                return v.round() as i64;
            }
            let out = o + v * f64::from(drv) / f64::from(sent);
            if !out.is_finite() {
                return 0;
            }
            (out.round() as i64).clamp(0, i64::from(drv) - 1)
        };
        (
            conv(x, self.sent.0, self.driver.0, self.origin.0),
            conv(y, self.sent.1, self.driver.1, self.origin.1),
        )
    }
}

/// Driver tools whose schema carries `from_zoom` (verified on 0.26.1
/// tools/list): after a `zoom`, x/y in the crop's pixel space ride
/// `from_zoom:true` and the driver translates them back to full-window
/// space — the only correct mapping, since the crop carries the
/// driver's own 20% padding we cannot reproduce locally (F3).
const FROM_ZOOM_TOOLS: &[&str] = &["click", "right_click", "double_click", "drag"];

/// Live driver session: the stdio client plus the per-window snapshot and
/// browser-tab bookkeeping later calls need.
pub struct Live {
    client: StdioClient,
    /// `ovs-<8 chars of the overseer session id>` (D2).
    session: String,
    /// One `check_permissions` probe per client lifetime (D6).
    perm_checked: bool,
    /// The frame the last screenshot established — the model's x/y.
    frame: Option<CoordFrame>,
    /// The last observation was a `zoom` crop: pointer x/y acts ride
    /// `from_zoom:true` until the next `screenshot` or `observe` (F3).
    zoomed: bool,
    /// (pid, window_id) → the last observation's snapshot handle and
    /// element-index → element_token map. A bare element_index is refused
    /// by the driver, so the token (preferred) or the snapshot id rides
    /// along on every element-targeted call.
    snaps: HashMap<(u64, u64), Snap>,
    /// Browser `tab_id` → `target_id` minted by the bind call.
    tabs: HashMap<String, String>,
}

struct Snap {
    id: Option<String>,
    tokens: HashMap<i64, String>,
    /// element_index → {x,y,w,h} bounds — only the live coordinate test
    /// reads centres off this.
    #[cfg(test)]
    frames: HashMap<i64, (f64, f64, f64, f64)>,
}

/// How a driver call failed — the distinction that decides whether the
/// client survives (refusal) or is dropped (transport).
enum CallErr {
    /// The transport died or lied — caller drops the client (respawn next).
    Transport(String),
    /// The driver refused/errored — the message is already model-facing.
    Refused(String),
}

impl std::fmt::Display for CallErr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CallErr::Transport(m) | CallErr::Refused(m) => f.write_str(m),
        }
    }
}

/// Spawn the driver child (`<path> mcp`) and run the MCP handshake. Called
/// lazily on the first computer call after boot or after a drop.
pub fn spawn(path: &Path, ctx: &ToolCtx) -> Result<Live, String> {
    let program = path.display().to_string();
    let mut client = StdioClient::spawn_with_env("cua-driver", &program, &["mcp".to_string()], &[])
        .map_err(|e| format!("computer: {e}"))?;
    client
        .initialize("overseer", env!("CARGO_PKG_VERSION"))
        .map_err(|e| format!("computer: {e}"))?;
    Ok(Live {
        client,
        session: session_label(ctx),
        perm_checked: false,
        frame: None,
        zoomed: false,
        snaps: HashMap::new(),
        tabs: HashMap::new(),
    })
}

/// `ovs-<8 chars of the overseer session id>` — the label the driver uses
/// to scope snapshots, browser targets and refs (D2).
fn session_label(ctx: &ToolCtx) -> String {
    let id = ctx
        .session_dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    let short: String = id.chars().take(8).collect();
    format!("ovs-{short}")
}

impl Live {
    /// `end_session` on registry drop — best-effort, the client's own Drop
    /// still kills the child when the call fails (D2).
    pub fn end_session(&mut self) {
        let _ = self.client.call_tool_with_timeout(
            "end_session",
            json!({ "session": self.session }),
            END_SESSION_TIMEOUT,
        );
    }
}

/// Dispatch one already-validated action through the driver. `Err` is the
/// honest, model-facing error string.
pub fn run(input: &Value, ctx: &ToolCtx, st: &mut super::ComputerState) -> Result<Value, String> {
    let mut live = match st.driver.take() {
        Some(live) => live,
        None => {
            let path = st
                .backends
                .driver
                .clone()
                .expect("driver dispatch requires Backends.driver");
            spawn(&path, ctx)?
        }
    };
    match run_live(&mut live, input, ctx) {
        Ok(v) => {
            st.driver = Some(live);
            Ok(v)
        }
        Err(CallErr::Refused(m)) => {
            st.driver = Some(live);
            Err(m)
        }
        // Transport died — drop the client so the next call respawns (D2).
        Err(CallErr::Transport(m)) => Err(format!(
            "computer: {m} — driver dropped; the next call respawns it"
        )),
    }
}

/// One `tools/call` with the session label injected; refusals and tool
/// errors become [`CallErr::Refused`] with a model-actionable message.
fn call(
    d: &mut Live,
    tool: &str,
    mut args: Map<String, Value>,
    req: &Value,
) -> Result<Value, CallErr> {
    args.insert("session".into(), json!(d.session));
    let result = d
        .client
        .call_tool_with_timeout(tool, Value::Object(args), CALL_TIMEOUT)
        .map_err(CallErr::Transport)?;
    if is_failure(&result) {
        return Err(CallErr::Refused(shape_failure(d, tool, &result, req)));
    }
    Ok(result)
}

fn is_failure(result: &Value) -> bool {
    result.get("isError").and_then(Value::as_bool) == Some(true)
        || result
            .pointer("/structuredContent/status")
            .and_then(Value::as_str)
            == Some("refused")
        || result.pointer("/structuredContent/refusal").is_some()
}

/// D6 failure messages — the refusal text is shaped so the model (or the
/// user) can act on it: stale element → re-observe, missing OS permission →
/// the grant command, browser refusal → the window fallback.
fn shape_failure(d: &mut Live, tool: &str, result: &Value, req: &Value) -> String {
    let sc = result
        .get("structuredContent")
        .cloned()
        .unwrap_or(Value::Null);
    let code = sc
        .pointer("/refusal/code")
        .or_else(|| sc.get("code"))
        .and_then(Value::as_str)
        .unwrap_or("");
    let detail = sc
        .pointer("/refusal/message")
        .or_else(|| sc.get("detail"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| shape::content_text(result));
    let hay = format!("{code} {detail}").to_lowercase();

    // Stale element indices / browser refs — the model must re-observe.
    if code == "stale_element_token" || code == "snapshot_id_required" || hay.contains("stale") {
        if let Some(r) = req.get("ref").and_then(Value::as_str) {
            return format!(
                "computer: browser ref {r} is stale — snapshot the tab again (browser)"
            );
        }
        let n = req
            .get("element")
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "(index)".into());
        return format!("computer: element {n} is stale — run observe again");
    }
    // Permission-shaped refusals: probe once per client, then the fixed
    // grant message the model relays to the user verbatim.
    if [
        "permission",
        "accessibility",
        "screen recording",
        "screen_recording",
        "tcc",
        "authoriz",
    ]
    .iter()
    .any(|m| hay.contains(m))
    {
        let mut msg = "computer: cua-driver lacks Accessibility/Screen Recording permission — \
             ask the user to run `cua-driver permissions grant`"
            .to_string();
        if !d.perm_checked {
            d.perm_checked = true;
            if let Some(missing) = check_permissions(d) {
                msg.push_str(&format!(" (driver reports missing: {missing})"));
            }
        }
        return msg;
    }
    // Browser-not-supported: pass the refusal through with the fallback.
    if tool.starts_with("browser") || tool == "get_browser_state" {
        let detail = if detail.is_empty() {
            "refused"
        } else {
            detail.as_str()
        };
        return format!(
            "computer: {tool} refused: {detail} — use observe/click on the window instead"
        );
    }
    if detail.is_empty() {
        format!("computer: {tool} failed")
    } else {
        format!("computer: {tool} failed: {detail}")
    }
}

/// `check_permissions` once per client (D6): the missing entries' key names.
fn check_permissions(d: &mut Live) -> Option<String> {
    let r = d
        .client
        .call_tool_with_timeout(
            "check_permissions",
            json!({ "session": d.session }),
            CALL_TIMEOUT,
        )
        .ok()?;
    let missing: Vec<String> = r
        .get("structuredContent")?
        .as_object()?
        .iter()
        .filter(|(_, v)| v.as_bool() == Some(false))
        .map(|(k, _)| k.clone())
        .collect();
    if missing.is_empty() {
        None
    } else {
        Some(missing.join(", "))
    }
}

/// The driver needs these params on this action — said so, with where the
/// model finds them (D6 spirit: an error the model can act on).
fn need(input: &Value, action: &str, keys: &[&str]) -> Result<(), CallErr> {
    for k in keys {
        if input.get(*k).is_none() {
            return Err(CallErr::Refused(format!(
                "computer: '{action}' needs '{k}' — 'apps' and 'windows' list them"
            )));
        }
    }
    Ok(())
}

/// Copy an integer-ish field to driver args verbatim (no key when absent).
fn put(args: &mut Map<String, Value>, key: &str, input: &Value) {
    if let Some(v) = input.get(key).filter(|v| v.is_number()) {
        args.insert(key.into(), v.clone());
    }
}

fn u64_of(input: &Value, key: &str) -> Option<u64> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .or_else(|| input.get(key).and_then(Value::as_i64).map(|v| v as u64))
        .or_else(|| input.get(key).and_then(Value::as_f64).map(|v| v as u64))
}

/// Element addressing (D3): the driver refuses a bare `element_index`, so
/// the last observation's `element_token` (preferred) or `snapshot_id` +
/// `element_index` rides along. Coordinates map through the recorded
/// [`CoordFrame`] — and after a `zoom`, pointer acts carry `from_zoom`
/// verbatim so the driver translates the crop space itself (F2/F3).
fn target(
    d: &Live,
    input: &Value,
    args: &mut Map<String, Value>,
    tool: &str,
) -> Result<(), CallErr> {
    if let Some(el) = input.get("element") {
        let Some(el) = u64_of(input, "element") else {
            return Err(CallErr::Refused(format!(
                "computer: 'element' must be a non-negative integer, got {el}"
            )));
        };
        let pid = u64_of(input, "pid").unwrap_or(0);
        let wid = u64_of(input, "window_id").unwrap_or(0);
        match d.snaps.get(&(pid, wid)) {
            Some(s) if s.tokens.contains_key(&(el as i64)) => {
                args.insert("element_token".into(), json!(s.tokens[&(el as i64)]));
            }
            Some(s) => {
                args.insert("element_index".into(), json!(el));
                if let Some(id) = &s.id {
                    args.insert("snapshot_id".into(), json!(id));
                }
            }
            None => {
                args.insert("element_index".into(), json!(el));
            }
        }
        return Ok(());
    }
    if let (Some(x), Some(y)) = (
        input.get("x").and_then(Value::as_f64),
        input.get("y").and_then(Value::as_f64),
    ) {
        if d.zoomed {
            if !FROM_ZOOM_TOOLS.contains(&tool) {
                return Err(CallErr::Refused(format!(
                    "computer: x/y in a zoom crop can't be translated for '{tool}' — take a fresh \
                     'screenshot' first"
                )));
            }
            // The driver owns the crop geometry (20% padding) — pass the
            // crop pixels through and let it translate (F3).
            args.insert("x".into(), json!(x));
            args.insert("y".into(), json!(y));
            args.insert("from_zoom".into(), json!(true));
            return Ok(());
        }
        let (nx, ny) = d
            .frame
            .as_ref()
            .map(|f| f.map(x, y))
            .unwrap_or_else(|| (x.round() as i64, y.round() as i64));
        args.insert("x".into(), json!(nx));
        args.insert("y".into(), json!(ny));
    }
    Ok(())
}

/// Dispatch the validated action to its driver call.
fn run_live(d: &mut Live, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    let action = action_of(input);
    match action.as_str() {
        "apps" => {
            let r = call(d, "list_apps", Map::new(), input)?;
            Ok(obs_envelope("apps", shape::apps(&r)))
        }
        "windows" => {
            let mut a = Map::new();
            put(&mut a, "pid", input);
            let r = call(d, "list_windows", a, input)?;
            Ok(obs_envelope("windows", shape::windows(&r)))
        }
        "launch" => {
            let mut a = Map::new();
            a.insert("name".into(), input["app"].clone());
            act(d, "launch_app", a, input, ctx)
        }
        "observe" => observe(d, input),
        "screenshot" => screenshot(d, input, ctx),
        "zoom" => zoom(d, input, ctx),
        "click" => click(d, input, ctx),
        "type" => {
            need(input, "type", &["pid"])?;
            let mut a = Map::new();
            put(&mut a, "pid", input);
            put(&mut a, "window_id", input);
            a.insert("text".into(), input["text"].clone());
            target(d, input, &mut a, "type_text")?;
            act(d, "type_text", a, input, ctx)
        }
        "key" => key(d, input, ctx),
        "set" => {
            need(input, "set", &["pid", "window_id"])?;
            let mut a = Map::new();
            put(&mut a, "pid", input);
            put(&mut a, "window_id", input);
            a.insert("value".into(), input["value"].clone());
            target(d, input, &mut a, "set_value")?;
            act(d, "set_value", a, input, ctx)
        }
        "scroll" => scroll(d, input, ctx),
        "drag" => drag(d, input, ctx),
        "menu" => {
            need(input, "menu", &["pid", "window_id"])?;
            let mut a = Map::new();
            put(&mut a, "pid", input);
            put(&mut a, "window_id", input);
            a.insert("path".into(), input["path"].clone());
            act(d, "invoke_menu", a, input, ctx)
        }
        "verify" => verify(d, input),
        "browser" => browser(d, input),
        "browser_click" | "browser_type" | "navigate" => browser_act(d, &action, input, ctx),
        other => Err(CallErr::Refused(format!(
            "computer: '{other}' is not a driver action"
        ))),
    }
}

/// `observe` — the AX element list, no pixels (D3/D4).
fn observe(d: &mut Live, input: &Value) -> Result<Value, CallErr> {
    need(input, "observe", &["pid", "window_id"])?;
    let limit = input
        .get("limit")
        .and_then(Value::as_u64)
        .unwrap_or(150)
        .clamp(1, 2000) as usize;
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    a.insert("include_screenshot".into(), json!(false));
    a.insert("max_elements".into(), json!(limit));
    if let Some(q) = input.get("query").and_then(Value::as_str) {
        if !q.is_empty() {
            a.insert("query".into(), json!(q));
        }
    }
    let r = call(d, "get_window_state", a, input)?;
    // A fresh full observation ends the zoom frame (F3).
    d.zoomed = false;
    // Remember the snapshot: `element` acts address through element_token
    // (preferred) or snapshot_id — a bare index is refused by the driver.
    let els = shape::parse_elements(&r);
    let key = (
        u64_of(input, "pid").unwrap_or(0),
        u64_of(input, "window_id").unwrap_or(0),
    );
    d.snaps.insert(
        key,
        Snap {
            id: r
                .pointer("/structuredContent/snapshot_id")
                .and_then(Value::as_str)
                .map(str::to_string),
            tokens: els
                .iter()
                .filter_map(|e| e.token.clone().map(|t| (e.index, t)))
                .collect(),
            #[cfg(test)]
            frames: els
                .iter()
                .filter_map(|e| e.frame.map(|f| (e.index, f)))
                .collect(),
        },
    );
    Ok(obs_envelope("observe", shape::elements(&r, limit)))
}

/// The capture suppression check (credential field / watch mode) — pixels
/// are never requested when suppressed.
fn is_suppressed(input: &Value, ctx: &ToolCtx, action: &str) -> bool {
    let cfg = ctx
        .agent_config
        .as_ref()
        .map(|c| c.computer.clone())
        .unwrap_or_default();
    crate::computer_obs::is_suppressed(&cfg, cred_field(input), action)
}

/// Metadata-only envelope for a suppressed (or pixel-less) capture.
fn suppressed_envelope(
    action: &str,
    ctx: &ToolCtx,
    reason: &str,
    suppressed: bool,
) -> Result<Value, CallErr> {
    let obs = read_obs(ctx);
    let (px_w, px_h, sent_w, sent_h) = obs
        .as_ref()
        .map(|o| (o.px_w, o.px_h, o.sent_w, o.sent_h))
        .unwrap_or((0, 0, 0, 0));
    let pre = obs.and_then(|o| o.sha256);
    Ok(json!({
        "ok": true,
        "computer": "screenshot",
        "action": action,
        "tier": "cua",
        "suppressed": suppressed,
        "reason": reason,
        "obs": crate::computer_obs::metadata_obs(px_w, px_h, sent_w, sent_h, reason),
        "pre": pre,
        "post": Value::Null,
    }))
}

/// `screenshot` — pixels only (no AX walk), capped at `max` (D3/D4).
fn screenshot(d: &mut Live, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    need(input, "screenshot", &["pid", "window_id"])?;
    if is_suppressed(input, ctx, "screenshot") {
        let reason = if ctx
            .agent_config
            .as_ref()
            .map(|c| c.computer.watch_mode)
            .unwrap_or(false)
        {
            "watch mode"
        } else {
            "credential-field focus"
        };
        return suppressed_envelope("screenshot", ctx, reason, true);
    }
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    a.insert("include_accessibility_tree".into(), json!(false));
    let max = input
        .get("max")
        .and_then(Value::as_u64)
        .unwrap_or(1280)
        .clamp(1, 8192);
    a.insert("max_dimension".into(), json!(max));
    let r = call(d, "get_window_state", a, input)?;
    capture_envelope(d, ctx, "screenshot", &r)
}

/// `zoom` — crop a window region (D3/F3). The crop becomes the model's
/// coordinate frame until the next `screenshot` or `observe`: pointer
/// acts then carry `from_zoom:true` so the driver translates crop pixels
/// back to full-window space itself (the crop has 20% padding only the
/// driver knows). The envelope tells the model which frame it is in.
fn zoom(d: &mut Live, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    need(input, "zoom", &["pid", "window_id"])?;
    if is_suppressed(input, ctx, "zoom") {
        return suppressed_envelope("zoom", ctx, "credential-field focus", true);
    }
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    for k in ["x1", "y1", "x2", "y2"] {
        if let Some(v) = input.get(k).filter(|v| v.is_number()) {
            a.insert(k.into(), v.clone());
        }
    }
    let r = call(d, "zoom", a, input)?;
    let mut env = capture_envelope(d, ctx, "zoom", &r)?;
    env["frame"] = json!(
        "zoom crop of the window — x/y acts take crop pixels (translated via from_zoom); \
         screenshot or observe returns to window space"
    );
    Ok(env)
}

/// A driver result's first image part → a persisted capture + the same
/// envelope shape the legacy path produced (`image_file`, sent/native
/// frame, pre/post digests) — so `image_block` and the audit event see no
/// difference (D4/D7). `px_*`/`sent_*` are the image's own dimensions —
/// the model sees what the driver returned, delivered unscaled (F2: the
/// `window_bounds` merge is gone; the window's native size rides along
/// only as informational `window_*`).
fn capture_envelope(
    d: &mut Live,
    ctx: &ToolCtx,
    action: &str,
    r: &Value,
) -> Result<Value, CallErr> {
    let sc = r.get("structuredContent").cloned().unwrap_or(Value::Null);
    let img = r
        .get("content")
        .and_then(Value::as_array)
        .and_then(|parts| {
            parts
                .iter()
                .find(|p| p.get("type").and_then(Value::as_str) == Some("image"))
        });
    let data = img
        .and_then(|p| p.get("data"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let media_type = img
        .and_then(|p| p.get("mimeType"))
        .and_then(Value::as_str)
        .or_else(|| sc.get("screenshot_mime_type").and_then(Value::as_str))
        .or_else(|| sc.get("mime_type").and_then(Value::as_str))
        .unwrap_or("image/png");
    let u32_sc = |keys: &[&str]| -> u32 {
        keys.iter()
            .find_map(|k| sc.get(*k).and_then(Value::as_u64))
            .unwrap_or(0) as u32
    };
    let (sent_w, sent_h) = if action == "zoom" {
        (u32_sc(&["width"]), u32_sc(&["height"]))
    } else {
        (
            u32_sc(&["screenshot_width"]),
            u32_sc(&["screenshot_height"]),
        )
    };
    // The delivered image's real pixel size — no window_bounds merge:
    // under the driver==sent rule the two are the same space (F2).
    let (px_w, px_h) = (sent_w, sent_h);
    let (window_w, window_h) = (
        sc.pointer("/window_bounds/width")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        sc.pointer("/window_bounds/height")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
    );
    let Some(data) = data else {
        return suppressed_envelope(
            "screenshot_or_zoom",
            ctx,
            "driver returned no pixels",
            false,
        );
    };
    let sha = digest_short(data);
    let pre = read_obs(ctx).and_then(|o| o.sha256);
    if action == "zoom" {
        // The crop becomes the model's frame until the next full
        // observation; pointer acts ride from_zoom (F3).
        d.zoomed = true;
    } else {
        // A fresh full screenshot: driver coords ARE sent coords (F2).
        d.frame = Some(CoordFrame::sent_only(sent_w, sent_h));
        d.zoomed = false;
        super::write_obs(
            ctx,
            &ObsState {
                px_w,
                px_h,
                sent_w,
                sent_h,
                sha256: Some(sha.clone()),
            },
        );
    }
    let dir = ctx.session_dir.join(super::CAPTURE_DIR);
    if let Err(e) = std::fs::create_dir_all(&dir) {
        return Err(CallErr::Refused(format!(
            "computer: cannot create {}: {e}",
            dir.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let file = dir.join(format!("capture-{}.json", uuid::Uuid::now_v7()));
    let capture = json!({
        "media_type": media_type,
        "data_b64": data,
        "px_w": px_w,
        "px_h": px_h,
        "sent_w": sent_w,
        "sent_h": sent_h,
    });
    if let Err(e) = write_owner_only(&file, &capture.to_string()) {
        return Err(CallErr::Refused(format!(
            "computer: cannot write {}: {e}",
            file.display()
        )));
    }
    Ok(json!({
        "ok": true,
        "computer": "screenshot",
        "action": action,
        "tier": "cua",
        "suppressed": false,
        "media_type": media_type,
        "image_file": file.display().to_string(),
        "px_w": px_w,
        "px_h": px_h,
        "sent_w": sent_w,
        "sent_h": sent_h,
        "window_w": window_w,
        "window_h": window_h,
        "pre": pre,
        "post": sha,
        "note": "send x/y in sent_w x sent_h pixels — the driver takes the same space \
         (image not persisted across resume)",
    }))
}

fn click(d: &mut Live, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    need(input, "click", &["pid"])?;
    let button = input
        .get("button")
        .and_then(Value::as_str)
        .unwrap_or("left")
        .to_ascii_lowercase();
    let count = input.get("count").and_then(Value::as_u64).unwrap_or(1);
    let tool = match (button.as_str(), count) {
        ("right", 2) => {
            return Err(CallErr::Refused(
                "computer: right double-click is not supported by the driver".into(),
            ))
        }
        ("right", _) => "right_click",
        (_, 2) => "double_click",
        _ => "click",
    };
    // api/name/role are helper vocabulary — the driver addresses by
    // element token or x/y only (F4).
    if input.get("element").is_none() && input.get("x").is_none() {
        return Err(CallErr::Refused(
            "computer: click by 'api'/'name'/'role' is helper vocabulary — under cua-driver \
             pass 'element' (from observe) or 'x'/'y'"
                .into(),
        ));
    }
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    target(d, input, &mut a, tool)?;
    if tool == "click" {
        if button != "left" {
            a.insert("button".into(), json!(button));
        }
        if count > 2 {
            a.insert("count".into(), json!(count));
        }
    }
    act(d, tool, a, input, ctx)
}

fn key(d: &mut Live, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    need(input, "key", &["pid"])?;
    let keys = input
        .get("keys")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    let (tool, a) = if keys.contains('+') {
        a.insert(
            "keys".into(),
            json!(keys
                .split('+')
                .map(|k| k.trim())
                .filter(|k| !k.is_empty())
                .collect::<Vec<_>>()),
        );
        ("hotkey", a)
    } else {
        a.insert("key".into(), json!(keys));
        ("press_key", a)
    };
    act(d, tool, a, input, ctx)
}

fn scroll(d: &mut Live, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    need(input, "scroll", &["pid"])?;
    let dx = input.get("dx").and_then(Value::as_f64).unwrap_or(0.0);
    let dy = input.get("dy").and_then(Value::as_f64).unwrap_or(0.0);
    if dx == 0.0 && dy == 0.0 {
        return Err(CallErr::Refused(
            "computer: 'scroll' needs a nonzero 'dx' or 'dy' (direction)".into(),
        ));
    }
    let (direction, amount) = if dy.abs() >= dx.abs() {
        (if dy < 0.0 { "up" } else { "down" }, dy.abs())
    } else {
        (if dx < 0.0 { "left" } else { "right" }, dx.abs())
    };
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    a.insert("direction".into(), json!(direction));
    a.insert(
        "amount".into(),
        json!(amount.round().clamp(1.0, 50.0) as u64),
    );
    target(d, input, &mut a, "scroll")?;
    act(d, "scroll", a, input, ctx)
}

fn drag(d: &mut Live, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    need(input, "drag", &["pid", "window_id"])?;
    let num = |k: &str| input.get(k).and_then(Value::as_f64).unwrap_or(0.0);
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    if d.zoomed {
        // drag carries from_zoom — pass both endpoints in crop pixels and
        // let the driver translate (F3).
        a.insert("from_x".into(), json!(num("x")));
        a.insert("from_y".into(), json!(num("y")));
        a.insert("to_x".into(), json!(num("to_x")));
        a.insert("to_y".into(), json!(num("to_y")));
        a.insert("from_zoom".into(), json!(true));
    } else {
        let (fx, fy) = d
            .frame
            .as_ref()
            .map(|f| f.map(num("x"), num("y")))
            .unwrap_or_else(|| (num("x").round() as i64, num("y").round() as i64));
        let (tx, ty) = d
            .frame
            .as_ref()
            .map(|f| f.map(num("to_x"), num("to_y")))
            .unwrap_or_else(|| (num("to_x").round() as i64, num("to_y").round() as i64));
        a.insert("from_x".into(), json!(fx));
        a.insert("from_y".into(), json!(fy));
        a.insert("to_x".into(), json!(tx));
        a.insert("to_y".into(), json!(ty));
    }
    act(d, "drag", a, input, ctx)
}

fn verify(d: &mut Live, input: &Value) -> Result<Value, CallErr> {
    need(input, "verify", &["pid", "window_id"])?;
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    // The driver's `expect` is a *list* of predicates; a bare object wraps.
    let expect = match input.get("expect") {
        Some(v @ Value::Array(_)) => v.clone(),
        Some(v) => json!([v]),
        None => Value::Null,
    };
    a.insert("expect".into(), expect);
    let r = call(d, "verify_state", a, input)?;
    let (satisfied, text) = shape::verify(&r);
    Ok(json!({
        "ok": true,
        "computer": "obs",
        "action": "verify",
        "tier": "cua",
        "satisfied": satisfied,
        "text": text,
    }))
}

/// `browser` — bind a window to a CDP target (`pid` + `window_id`), or
/// re-snapshot an already-bound `tab`. Either way the model gets the ref
/// list back.
fn browser(d: &mut Live, input: &Value) -> Result<Value, CallErr> {
    if let Some(tab) = input.get("tab").and_then(Value::as_str) {
        let Some(target) = d.tabs.get(tab).cloned() else {
            return Err(CallErr::Refused(format!(
                "computer: browser tab '{tab}' is not bound — call 'browser' with pid + window_id first"
            )));
        };
        return browser_snapshot(d, input, &target, tab);
    }
    need(input, "browser", &["pid", "window_id"])?;
    let mut a = Map::new();
    put(&mut a, "pid", input);
    put(&mut a, "window_id", input);
    let r = call(d, "get_browser_state", a, input)?;
    let Some((target, tab)) = shape::target_and_tab(&r) else {
        return Err(CallErr::Refused(
            "computer: get_browser_state returned no target_id/tab_id — is this a Chrome/Edge window?"
                .into(),
        ));
    };
    d.tabs.insert(tab.clone(), target.clone());
    browser_snapshot(d, input, &target, &tab)
}

fn browser_snapshot(
    d: &mut Live,
    input: &Value,
    target: &str,
    tab: &str,
) -> Result<Value, CallErr> {
    let mut a = Map::new();
    a.insert("target_id".into(), json!(target));
    a.insert("tab_id".into(), json!(tab));
    a.insert("snapshot_format".into(), json!("semantic_v2"));
    if let Some(q) = input.get("query").and_then(Value::as_str) {
        if !q.is_empty() {
            a.insert("query".into(), json!(q));
        }
    }
    let r = call(d, "get_browser_state", a, input)?;
    let mut env = obs_envelope("browser", shape::browser_refs(&r, 150));
    env["tab"] = json!(tab);
    Ok(env)
}

/// browser_click / browser_type / navigate — all addressed by `tab`.
fn browser_act(d: &mut Live, action: &str, input: &Value, ctx: &ToolCtx) -> Result<Value, CallErr> {
    let tab = input.get("tab").and_then(Value::as_str).unwrap_or("");
    let Some(target) = d.tabs.get(tab).cloned() else {
        return Err(CallErr::Refused(format!(
            "computer: browser tab '{tab}' is not bound — call 'browser' with pid + window_id first"
        )));
    };
    let tool = match action {
        "navigate" => "browser_navigate",
        other => other,
    };
    let mut a = Map::new();
    a.insert("target_id".into(), json!(target));
    a.insert("tab_id".into(), json!(tab));
    match action {
        "browser_click" => {
            if let Some(r) = input.get("ref").and_then(Value::as_str) {
                a.insert("ref".into(), json!(r));
            } else {
                for k in ["x", "y"] {
                    if let Some(v) = input.get(k).filter(|v| v.is_number()) {
                        a.insert(k.into(), v.clone());
                    }
                }
            }
        }
        "browser_type" => {
            a.insert("text".into(), input["text"].clone());
            if let Some(r) = input.get("ref").and_then(Value::as_str) {
                a.insert("ref".into(), json!(r));
            }
        }
        "navigate" => {
            a.insert("url".into(), input["url"].clone());
        }
        _ => {}
    }
    act(d, tool, a, input, ctx)
}

/// The observation envelope: compact rendered text plus the audit fields
/// (`computer` + `action` + `tier`) that `audit_event` reads (D7).
fn obs_envelope(action: &str, text: String) -> Value {
    json!({
        "ok": true,
        "computer": "obs",
        "action": action,
        "tier": "cua",
        "text": text,
    })
}

/// The act envelope — same audit contract as the helper path, plus the
/// driver's own confirmation line as `detail`. `pre` is the last
/// observation's digest; the driver reports no post digest.
fn act(
    d: &mut Live,
    tool: &str,
    a: Map<String, Value>,
    input: &Value,
    ctx: &ToolCtx,
) -> Result<Value, CallErr> {
    let r = call(d, tool, a, input)?;
    let detail = {
        let t = shape::content_text(&r);
        if t.is_empty() {
            "ok".to_string()
        } else {
            t
        }
    };
    Ok(json!({
        "ok": true,
        "computer": "act",
        "action": action_of(input),
        "tier": "cua",
        "driver": tool,
        "detail": detail,
        "pre": read_obs(ctx).and_then(|o| o.sha256),
        "post": Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overseer-cua-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(dir: &Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: None,
            sandbox: false,
            broker: None,
        }
    }

    /// On this VM, exec'ing a script that was just written intermittently
    /// returns ETXTBSY — retry the call for a few seconds.
    fn run_ok(
        input: &Value,
        c: &ToolCtx,
        st: &mut super::super::ComputerState,
    ) -> Result<Value, String> {
        let mut last = Err(String::new());
        for _ in 0..60 {
            match run(input, c, st) {
                Err(e) if e.contains("Text file busy") => {
                    last = Err(e);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
                other => return other,
            }
        }
        last
    }

    /// Write an executable fake-driver script (`<path> mcp`): a POSIX-sh
    /// JSON-RPC stdio server answering initialize/tools/list/tools/call
    /// with canned results. Every request line is appended to `log` so
    /// tests can see the session label and the arguments verbatim; a
    /// `spawn:` line per process proves respawn counts; `env-leak` appears
    /// only if the forbidden parent env var reached the child.
    fn fake_driver(dir: &Path, name: &str, extra_cases: &str) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join(format!("{name}.log"));
        let path = dir.join(name);
        let script = format!(
            r#"#!/bin/sh
LOG='{log}'
printf 'spawn:%s\n' "$*" >> "$LOG"
if env | grep -q OVERSEER_FAKE_CUA_SECRET; then printf 'env-leak\n' >> "$LOG"; fi
sid() {{ printf '%s' "$1" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/' | head -1; }}
tool() {{ printf '%s' "$1" | grep -o '"name":"[^"]*"' | tail -1 | cut -d'"' -f4; }}
while IFS= read -r line; do
  printf 'req %s\n' "$line" >> "$LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$(sid "$line"),\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"fake-cua\",\"version\":\"0\"}}}}}}"
      ;;
    *'"method":"notifications/initialized"'*) : ;;
    *'"method":"tools/list"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$(sid "$line"),\"result\":{{\"tools\":[]}}}}"
      ;;
    *'"method":"tools/call"'*)
      n=$(tool "$line")
      case "$n" in
        list_apps)
          r='{{"content":[{{"type":"text","text":"Found 2 apps"}}],"structuredContent":{{"apps":[{{"pid":11,"name":"TextEdit","running":true,"active":true}},{{"pid":12,"name":"kworker","running":true,"active":false}},{{"pid":13,"name":"NotRunning","running":false}}]}}}}'
          ;;
        list_windows)
          r='{{"content":[{{"type":"text","text":"1 window"}}],"structuredContent":{{"windows":[{{"window_id":101,"pid":11,"app_name":"TextEdit","title":"Doc","x":0,"y":0,"width":800,"height":600,"bounds":{{"x":0,"y":0,"width":800,"height":600}},"z_index":0,"is_on_screen":true}}]}}}}'
          ;;
        get_window_state)
          case "$line" in
            *'"include_accessibility_tree":false'*)
              r='{{"content":[{{"type":"image","data":"aGVsbG8=","mimeType":"image/png"}}],"structuredContent":{{"screenshot_width":1280,"screenshot_height":800,"screenshot_mime_type":"image/png","window_bounds":{{"x":0,"y":0,"width":2560,"height":1600}},"window_id":101,"window_title":"Doc","app_name":"TextEdit","pid":11}}}}'
              ;;
            *)
              r='{{"content":[{{"type":"text","text":"tree"}}],"structuredContent":{{"snapshot_id":"snap-1","elements":[{{"element_index":0,"element_token":"s00000001:0","role":"AXWindow","label":"","value":null,"actions":[],"depth":0,"parent_index":null}},{{"element_index":1,"element_token":"s00000001:1","role":"AXTextField","label":"Name","value":"hi","actions":["AXPress"],"depth":1,"parent_index":0}},{{"element_index":2,"element_token":"s00000001:2","role":"AXButton","label":"Go","value":null,"actions":["AXPress"],"depth":1,"parent_index":0}}],"element_count":3,"returned_element_count":3,"total_element_count":3,"elements_complete":true,"degraded":false,"window_id":101,"pid":11}}}}'
              ;;
          esac
          ;;
        zoom)
          r='{{"content":[{{"type":"image","data":"aGVsbG8=","mimeType":"image/jpeg"}}],"structuredContent":{{"format":"jpeg","width":300,"height":200,"mime_type":"image/jpeg","window_id":101,"pid":11}}}}'
          ;;
        verify_state)
          r='{{"content":[{{"type":"text","text":"verify_state: unknown after 1 sample(s) in 5 ms"}}],"structuredContent":{{"status":"unknown","predicates":[{{"index":0,"status":"unknown","observed_json":null,"unknown_reason":"untrusted_source"}}],"samples":1,"stable":false,"elapsed_ms":5}}}}'
          ;;
        get_browser_state)
          case "$line" in
            *'"target_id"'*)
              r='{{"content":[{{"type":"text","text":"2 refs"}}],"structuredContent":{{"target_id":"t-1","tab_id":"tab-1","refs":[{{"ref":"p1:0","role":"link","name":"Home"}},{{"ref":"p1:1","role":"button","name":"Go","value":"go"}}]}}}}'
              ;;
            *)
              r='{{"content":[{{"type":"text","text":"bound"}}],"structuredContent":{{"target_id":"t-1","tab_id":"tab-1"}}}}'
              ;;
          esac
          ;;
        click | right_click | double_click)
          case "$line" in
            *'"element_index":99'*)
              r='{{"isError":true,"content":[{{"type":"text","text":"refused (stale_element_token): element_token is stale"}}],"structuredContent":{{"refusal":{{"code":"stale_element_token","message":"element_token is stale; call get_window_state again to refresh"}},"status":"refused"}}}}'
              ;;
            *)
              r='{{"content":[{{"type":"text","text":"Clicked"}}],"structuredContent":{{"delivery":{{"mode":"background"}},"effect":"clicked","route":"ax"}}}}'
              ;;
          esac
          ;;
        check_permissions)
          r='{{"content":[{{"type":"text","text":"perms"}}],"structuredContent":{{"atspi":false,"x11":true,"wayland":false}}}}'
          ;;
        end_session)
          r='{{"content":[{{"type":"text","text":"bye"}}],"structuredContent":{{"ended":true}}}}'
          ;;
{extra_cases}
        *)
          r='{{"content":[{{"type":"text","text":"Done"}}],"structuredContent":{{"delivery":{{"mode":"background"}},"effect":"done","route":"ax"}}}}'
          ;;
      esac
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$(sid "$line"),\"result\":$r}}"
      ;;
    *)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$(sid "$line"),\"error\":{{\"code\":-32601,\"message\":\"method not found\"}}}}"
      ;;
  esac
done
"#,
            log = log.display()
        );
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(script.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        (path, log)
    }

    /// A driver that dies after answering ONE tools/call — the next call
    /// hits a closed transport and must respawn (D2).
    fn flaky_driver(dir: &Path) -> (PathBuf, PathBuf) {
        use std::os::unix::fs::PermissionsExt;
        let log = dir.join("flaky.log");
        let path = dir.join("flaky-cua.sh");
        let script = format!(
            r#"#!/bin/sh
LOG='{log}'
printf 'spawn:%s\n' "$*" >> "$LOG"
sid() {{ printf '%s' "$1" | sed 's/.*"id":\([0-9][0-9]*\).*/\1/' | head -1; }}
answered=0
while IFS= read -r line; do
  printf 'req %s\n' "$line" >> "$LOG"
  case "$line" in
    *'"method":"initialize"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$(sid "$line"),\"result\":{{\"protocolVersion\":\"2025-06-18\",\"capabilities\":{{}},\"serverInfo\":{{\"name\":\"flaky\",\"version\":\"0\"}}}}}}"
      ;;
    *'"method":"notifications/initialized"'*) : ;;
    *'"method":"tools/call"'*)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$(sid "$line"),\"result\":{{\"content\":[{{\"type\":\"text\",\"text\":\"ok\"}}]}}}}"
      answered=$((answered + 1))
      if [ "$answered" -ge 1 ]; then exit 0; fi
      ;;
    *)
      printf '%s\n' "{{\"jsonrpc\":\"2.0\",\"id\":$(sid "$line"),\"error\":{{\"code\":-32601,\"message\":\"nope\"}}}}"
      ;;
  esac
done
"#,
            log = log.display()
        );
        {
            use std::io::Write;
            let mut f = std::fs::File::create(&path).unwrap();
            f.write_all(script.as_bytes()).unwrap();
            f.sync_all().unwrap();
        }
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        (path, log)
    }

    /// A state pinned at the fake driver — no env probing in tests.
    fn state(driver: &Path) -> super::super::ComputerState {
        super::super::ComputerState::new(super::super::Backends {
            driver: Some(driver.to_path_buf()),
            ..Default::default()
        })
    }

    fn req_lines(log: &Path) -> Vec<String> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("req "))
            .map(|l| l[4..].to_string())
            .collect()
    }

    fn spawn_count(log: &Path) -> usize {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("spawn:"))
            .count()
    }

    #[test]
    fn driver_spawns_lazily_and_sends_the_session_label() {
        let dir = tmpdir("lazy");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        // Laziness: state construction must not spawn.
        assert!(st.driver.is_none());
        assert!(!log.exists(), "driver spawned before first use");
        let c = ctx(&dir);
        let out = run_ok(&json!({"action": "apps"}), &c, &mut st).unwrap();
        assert!(st.driver.is_some());
        let text = out["text"].as_str().unwrap();
        assert!(text.contains("11  TextEdit  [front]"), "{text}");
        assert!(text.contains("12  kworker"), "{text}");
        assert!(!text.contains("NotRunning"), "{text}");
        // Every request (initialize excluded) carries the session label.
        let label = format!("ovs-{}", "session");
        for req in req_lines(&log) {
            if req.contains("\"method\":\"tools/call\"") {
                assert!(
                    req.contains(&format!("\"session\":\"{label}\"")),
                    "call missing session label: {req}"
                );
            }
        }
        assert!(
            std::fs::read_to_string(&log).unwrap().contains("spawn:mcp"),
            "spawned as `<driver> mcp`"
        );
        assert_eq!(out["tier"], "cua");
        assert_eq!(out["action"], "apps");
    }

    #[test]
    fn driver_child_env_hides_parent_secrets() {
        let dir = tmpdir("env");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        std::env::set_var("OVERSEER_FAKE_CUA_SECRET", "leak-me");
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(&json!({"action": "apps"}), &c, &mut st).unwrap();
        std::env::remove_var("OVERSEER_FAKE_CUA_SECRET");
        let log = std::fs::read_to_string(&log).unwrap();
        assert!(
            !log.contains("env-leak"),
            "secret env var reached the driver: {log}"
        );
    }

    #[test]
    fn observe_renders_elements_and_remembers_tokens() {
        let dir = tmpdir("observe");
        let (driver, _log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        let out = run(
            &json!({"action": "observe", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        let text = out["text"].as_str().unwrap();
        assert!(
            text.contains("[1] AXTextField \"Name\" = \"hi\"  {AXPress}"),
            "{text}"
        );
        assert!(text.contains("[2] AXButton \"Go\"  {AXPress}"), "{text}");
        // The unlabeled root container survives (kept children) — elements
        // tokens are stored for later addressing.
        let d = st.driver.as_ref().unwrap();
        let snap = d.snaps.get(&(11, 101)).expect("snapshot stored");
        assert_eq!(snap.id.as_deref(), Some("snap-1"));
        assert_eq!(snap.tokens.get(&1).map(String::as_str), Some("s00000001:1"));
    }

    #[test]
    fn observe_truncates_and_says_how_to_narrow() {
        let dir = tmpdir("trunc");
        let (driver, _log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        let out = run(
            &json!({"action": "observe", "pid": 11, "window_id": 101, "limit": 1}),
            &c,
            &mut st,
        )
        .unwrap();
        let text = out["text"].as_str().unwrap();
        assert!(text.contains("more — narrow with query"), "{text}");
    }

    #[test]
    fn screenshot_returns_a_capture_envelope_and_records_the_frame() {
        let dir = tmpdir("shot");
        let (driver, _log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        let out = run(
            &json!({"action": "screenshot", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        assert_eq!(out["computer"], "screenshot");
        assert_eq!(out["tier"], "cua");
        // F2: px_* is the delivered image's real size — sent == driver
        // space; the native window dims are informational only.
        assert_eq!(out["sent_w"], 1280);
        assert_eq!(out["px_w"], 1280);
        assert_eq!(out["window_w"], 2560);
        let file = out["image_file"].as_str().unwrap().to_string();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["data_b64"], "aGVsbG8=");
        // The frame is remembered — and the agent-side image block decodes
        // the same file.
        let obs = read_obs(&c).unwrap();
        assert_eq!((obs.px_w, obs.sent_w), (1280, 1280));
        let block = super::super::image_block(&out.to_string()).expect("image block");
        match block {
            crate::ir::Block::Image {
                media_type,
                data_b64,
                sent_w,
                px_w,
                ..
            } => {
                assert_eq!(media_type, "image/png");
                assert_eq!(data_b64, "aGVsbG8=");
                assert_eq!((sent_w, px_w), (1280, 1280));
            }
            other => panic!("expected image block, got {other:?}"),
        }
        // The audit contract is the same as the helper path's (D7).
        let ev = super::super::audit_event(&out.to_string()).expect("audit event");
        match ev {
            crate::event::EventKind::ComputerAct { action, tier, .. } => {
                assert_eq!(action, "screenshot");
                assert_eq!(tier, "cua");
            }
            other => panic!("expected ComputerAct, got {other:?}"),
        }
    }

    #[test]
    fn click_by_element_sends_the_remembered_token() {
        let dir = tmpdir("click-el");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(
            &json!({"action": "observe", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        let out = run(
            &json!({"action": "click", "pid": 11, "window_id": 101, "element": 1}),
            &c,
            &mut st,
        )
        .unwrap();
        assert_eq!(out["computer"], "act");
        assert_eq!(out["tier"], "cua");
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("\"element_token\":\"s00000001:1\""), "{last}");
        assert!(
            !last.contains("\"x\":"),
            "element click must not send coords: {last}"
        );
    }

    #[test]
    fn click_by_xy_passes_sent_frame_coords_verbatim() {
        // F2: the driver's x/y space IS the delivered screenshot's pixel
        // space — a click at (640,400) on the 1280x800 image is sent
        // verbatim even though the window is 2560x1600 natively.
        let dir = tmpdir("click-xy");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(
            &json!({"action": "screenshot", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        run_ok(
            &json!({"action": "click", "pid": 11, "window_id": 101, "x": 640.0, "y": 400.0}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("\"x\":640"), "no local scaling: {last}");
        assert!(last.contains("\"y\":400"), "no local scaling: {last}");
        assert!(!last.contains("from_zoom"), "{last}");
        // No observation recorded → still 1:1 pass-through.
        let dir2 = tmpdir("click-raw");
        let (driver2, log2) = fake_driver(&dir2, "fake.sh", "");
        let mut st2 = state(&driver2);
        let c2 = ctx(&dir2);
        run_ok(
            &json!({"action": "click", "pid": 11, "x": 64.0, "y": 40.0}),
            &c2,
            &mut st2,
        )
        .unwrap();
        let last = req_lines(&log2).last().unwrap().clone();
        assert!(last.contains("\"x\":64"), "{last}");
    }

    #[test]
    fn zoom_then_pointer_acts_ride_from_zoom() {
        // F3: after zoom, x/y acts pass the crop pixels through with
        // from_zoom — the driver translates back to full-window space.
        let dir = tmpdir("zoom");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        let out = run(
            &json!({"action": "zoom", "pid": 11, "window_id": 101,
                    "x1": 10.0, "y1": 10.0, "x2": 50.0, "y2": 40.0}),
            &c,
            &mut st,
        )
        .unwrap();
        assert_eq!(out["computer"], "screenshot");
        assert_eq!(out["sent_w"], 300, "crop dims are the sent frame");
        assert!(
            out["frame"].as_str().unwrap().contains("from_zoom"),
            "envelope names the frame: {}",
            out["frame"]
        );
        // A click in crop pixels rides from_zoom verbatim.
        run_ok(
            &json!({"action": "click", "pid": 11, "window_id": 101, "x": 30.0, "y": 15.0}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("\"from_zoom\":true"), "{last}");
        assert!(last.contains("\"x\":30"), "crop coords verbatim: {last}");
        // Element acts are unaffected — the token path needs no coords.
        run_ok(
            &json!({"action": "observe", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        run_ok(
            &json!({"action": "click", "pid": 11, "window_id": 101, "element": 2}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("element_token"), "{last}");
    }

    #[test]
    fn zoom_frame_ends_at_the_next_full_observation() {
        let dir = tmpdir("zoomend");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(
            &json!({"action": "zoom", "pid": 11, "window_id": 101,
                    "x1": 10.0, "y1": 10.0, "x2": 50.0, "y2": 40.0}),
            &c,
            &mut st,
        )
        .unwrap();
        // A non-pointer x/y act cannot translate the crop: refused.
        let err = run(
            &json!({"action": "type", "pid": 11, "text": "a", "x": 5.0, "y": 5.0}),
            &c,
            &mut st,
        )
        .unwrap_err();
        assert!(err.contains("fresh 'screenshot'"), "{err}");
        // A fresh screenshot returns the frame to window space.
        run_ok(
            &json!({"action": "screenshot", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        run_ok(
            &json!({"action": "click", "pid": 11, "window_id": 101, "x": 30.0, "y": 15.0}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(!last.contains("from_zoom"), "{last}");
        assert!(last.contains("\"x\":30"), "{last}");
    }

    #[test]
    fn key_dispatches_hotkey_or_press_key() {
        let dir = tmpdir("key");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(
            &json!({"action": "key", "pid": 11, "keys": "cmd+s"}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("\"name\":\"hotkey\""), "{last}");
        assert!(last.contains("\"keys\":[\"cmd\",\"s\"]"), "{last}");
        run_ok(
            &json!({"action": "key", "pid": 11, "keys": "return"}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("\"name\":\"press_key\""), "{last}");
        assert!(last.contains("\"key\":\"return\""), "{last}");
    }

    #[test]
    fn verify_unknown_is_reported_not_satisfied() {
        let dir = tmpdir("verify");
        let (driver, _log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        let out = run(
            &json!({"action": "verify", "pid": 11, "window_id": 101,
                    "expect": {"window": {"exists": true}}}),
            &c,
            &mut st,
        )
        .unwrap();
        assert_eq!(out["satisfied"], false);
        assert!(out["text"].as_str().unwrap().contains("unknown"));
    }

    #[test]
    fn browser_binds_then_snapshots_and_clicks_by_ref() {
        let dir = tmpdir("browser");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        let out = run(
            &json!({"action": "browser", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        assert_eq!(out["tab"], "tab-1");
        let text = out["text"].as_str().unwrap();
        assert!(text.contains("p1:0  link \"Home\""), "{text}");
        // Two get_browser_state calls: bind (pid+window_id), then snapshot
        // (target_id+tab_id, semantic_v2).
        let calls: Vec<String> = req_lines(&log)
            .into_iter()
            .filter(|l| l.contains("get_browser_state"))
            .collect();
        assert_eq!(calls.len(), 2, "{calls:?}");
        assert!(calls[1].contains("\"target_id\":\"t-1\""), "{}", calls[1]);
        assert!(
            calls[1].contains("\"snapshot_format\":\"semantic_v2\""),
            "{}",
            calls[1]
        );
        // Click by ref rides the remembered target.
        run_ok(
            &json!({"action": "browser_click", "tab": "tab-1", "ref": "p1:1"}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("\"name\":\"browser_click\""), "{last}");
        assert!(last.contains("\"target_id\":\"t-1\""), "{last}");
        assert!(last.contains("\"ref\":\"p1:1\""), "{last}");
    }

    #[test]
    fn navigate_sends_the_url_to_the_bound_tab() {
        let dir = tmpdir("navigate");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(
            &json!({"action": "browser", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        run_ok(
            &json!({"action": "navigate", "tab": "tab-1", "url": "https://example.com"}),
            &c,
            &mut st,
        )
        .unwrap();
        let last = req_lines(&log).last().unwrap().clone();
        assert!(last.contains("\"name\":\"browser_navigate\""), "{last}");
        assert!(last.contains("\"url\":\"https://example.com\""), "{last}");
        // An unbound tab is refused before any driver call.
        let err = run(
            &json!({"action": "navigate", "tab": "tab-9", "url": "https://x"}),
            &c,
            &mut st,
        )
        .unwrap_err();
        assert!(err.contains("not bound"), "{err}");
    }

    #[test]
    fn stale_element_says_run_observe_again() {
        let dir = tmpdir("stale");
        let (driver, _log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(
            &json!({"action": "observe", "pid": 11, "window_id": 101}),
            &c,
            &mut st,
        )
        .unwrap();
        // element 99 has no remembered token → element_index+snapshot_id;
        // the fake refuses it as stale.
        let err = run(
            &json!({"action": "click", "pid": 11, "window_id": 101, "element": 99}),
            &c,
            &mut st,
        )
        .unwrap_err();
        assert!(err.contains("is stale — run observe again"), "{err}");
    }

    #[test]
    fn a_dead_transport_drops_the_driver_and_the_next_call_respawns() {
        let dir = tmpdir("respawn");
        let (driver, log) = flaky_driver(&dir);
        let mut st = state(&driver);
        let c = ctx(&dir);
        // First call works — the fake exits right after answering.
        run_ok(&json!({"action": "apps"}), &c, &mut st).unwrap();
        // Second call hits the dead pipe: error, client dropped.
        let err = run_ok(&json!({"action": "apps"}), &c, &mut st).unwrap_err();
        assert!(err.contains("respawns"), "{err}");
        assert!(st.driver.is_none(), "dead client must be dropped");
        // Third call respawns and works.
        run_ok(&json!({"action": "apps"}), &c, &mut st).unwrap();
        assert_eq!(spawn_count(&log), 2, "two spawns expected");
    }

    #[test]
    fn permission_refusal_names_the_grant_command_once() {
        let dir = tmpdir("perm");
        let extra = r#"        press_key)
          r='{"isError":true,"content":[{"type":"text","text":"refused (accessibility_permission_denied): macOS Accessibility permission is required"}],"structuredContent":{"refusal":{"code":"accessibility_permission_denied","message":"macOS Accessibility permission is required"},"status":"refused"}}'
          ;;
"#;
        let (driver, _log) = fake_driver(&dir, "fake.sh", extra);
        let mut st = state(&driver);
        let c = ctx(&dir);
        let err = run(
            &json!({"action": "key", "pid": 11, "keys": "a"}),
            &c,
            &mut st,
        )
        .unwrap_err();
        assert!(
            err.contains("cua-driver permissions grant"),
            "D6 message: {err}"
        );
        assert!(err.contains("atspi"), "probe detail appended: {err}");
    }

    #[test]
    fn batch_members_run_through_the_driver() {
        let dir = tmpdir("batch");
        let (driver, log) = fake_driver(&dir, "fake.sh", "");
        let mut st = state(&driver);
        let c = ctx(&dir);
        let mut c = c;
        let out = super::super::run_with(
            &json!({"action": "batch", "actions": [
                {"action": "observe", "pid": 11, "window_id": 101},
                {"action": "click", "pid": 11, "window_id": 101, "element": 2},
                {"action": "type", "pid": 11, "text": "hi"}
            ]}),
            &mut c,
            &mut st,
        )
        .unwrap();
        assert_eq!(out["computer"], "batch");
        assert_eq!(out["tier"], "cua");
        assert_eq!(out["count"], 3);
        let names: Vec<String> = req_lines(&log)
            .into_iter()
            .filter(|l| l.contains("\"method\":\"tools/call\""))
            .collect();
        assert_eq!(names.len(), 3);
        // The click member addressed the element through its token.
        assert!(
            names[1].contains("\"element_token\":\"s00000001:2\""),
            "{}",
            names[1]
        );
    }

    #[test]
    fn spec_is_byte_identical_with_or_without_a_driver() {
        // D1: the spec never depends on which backend is present.
        let dir = tmpdir("spec");
        let (driver, _log) = fake_driver(&dir, "fake.sh", "");
        let mut with = super::super::super::ToolRegistry::core_with(
            crate::perm::Policy::allow_all(),
            super::super::super::Optional::ALL,
        );
        with.computer = super::super::ComputerState::new(super::super::Backends {
            driver: Some(driver),
            ..Default::default()
        });
        let mut without = super::super::super::ToolRegistry::core_with(
            crate::perm::Policy::allow_all(),
            super::super::super::Optional::ALL,
        );
        without.computer = super::super::ComputerState::new(super::super::Backends::default());
        let spec_of = |reg: &super::super::super::ToolRegistry| {
            let spec = reg.specs.iter().find(|s| s.name == "computer").unwrap();
            format!(
                "{}{}",
                spec.description,
                serde_json::to_string(&spec.input_schema).unwrap()
            )
        };
        assert_eq!(spec_of(&with), spec_of(&without));
    }

    /// Live smoke (owner's Mac, cua-driver installed):
    /// `cargo test -p overseer-core -- --ignored computer_live`.
    /// Launches TextEdit in the background, observes its window, types
    /// "overseer", verifies the text landed, and closes without saving.
    #[test]
    #[ignore]
    fn computer_live() {
        if !cfg!(target_os = "macos") {
            eprintln!("computer_live is a macOS smoke test");
            return;
        }
        let Some(driver) = super::super::super::struct_search::find_on_path(&["cua-driver"]) else {
            eprintln!("cua-driver not on PATH — skipping");
            return;
        };
        let dir = std::env::temp_dir().join(format!("overseer-cua-live-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut st = state(&driver);
        let c = ctx(&dir);
        run_ok(&json!({"action": "launch", "app": "TextEdit"}), &c, &mut st).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(2));
        let wins = run_ok(&json!({"action": "windows"}), &c, &mut st).unwrap();
        let text = wins["text"].as_str().unwrap();
        let line = text.lines().find(|l| l.contains("TextEdit")).expect(text);
        let mut it = line.split_whitespace();
        let wid: u64 = it.next().unwrap().parse().unwrap();
        let pid: u64 = it.next().unwrap().parse().unwrap();
        run_ok(
            &json!({"action": "observe", "pid": pid, "window_id": wid}),
            &c,
            &mut st,
        )
        .unwrap();
        run_ok(
            &json!({"action": "type", "pid": pid, "window_id": wid, "text": "overseer"}),
            &c,
            &mut st,
        )
        .unwrap();
        let v = run(
            &json!({"action": "verify", "pid": pid, "window_id": wid,
                    "expect": {"element": {"selector": {"role": "AXTextArea"},
                                "value_equals": "overseer"}}}),
            &c,
            &mut st,
        )
        .unwrap();
        assert_eq!(v["satisfied"], true, "{v}");
        // F2 empirical check: an element's frame centre clicked by x/y
        // lands in the element — proving driver coords == sent coords.
        let (cx, cy) = {
            let d = st.driver.as_ref().unwrap();
            let snap = d.snaps.get(&(pid, wid)).expect("snapshot stored");
            let &(x, y, w, h) = snap.frames.values().next().expect("an element frame");
            (x + w / 2.0, y + h / 2.0)
        };
        run_ok(
            &json!({"action": "click", "pid": pid, "window_id": wid, "x": cx, "y": cy}),
            &c,
            &mut st,
        )
        .unwrap();
        let v = run(
            &json!({"action": "verify", "pid": pid, "window_id": wid,
                    "expect": {"element": {"selector": {"role": "AXTextArea"},
                                "exists": true, "value_equals": "overseer"}}}),
            &c,
            &mut st,
        )
        .unwrap();
        assert_eq!(v["satisfied"], true, "{v}");
        // Close without saving — kill is the safest no-save path.
        let _ = std::process::Command::new("kill")
            .arg(pid.to_string())
            .status();
    }
}
