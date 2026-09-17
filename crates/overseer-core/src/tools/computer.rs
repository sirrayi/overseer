//! computer tool (P7-3) — tiered computer-use dispatch.
//!
//! Capability tiers, best first: a **structured API** (app/browser
//! automation endpoint) is cheaper and more deterministic than an
//! **accessibility** lookup by element name/role, which beats a blind
//! **pixel** act at screen coordinates. `choose()` walks that order and
//! takes the first tier the operator has configured. A structured request
//! never silently degrades into a pixel act — if the tier that can express
//! it is unconfigured the call fails honestly.
//!
//! The engine links no OS framework: every backend is an opt-in helper
//! process named by an environment variable (the platform bindings live
//! there, not here). A missing backend is an `unconfigured` error naming
//! the variable to set — never a fake success, never a quiet downgrade.
//!
//! | tier        | variable                       |
//! |-------------|--------------------------------|
//! | structured  | `OVERSEER_COMPUTER_STRUCTURED` |
//! | a11y        | `OVERSEER_COMPUTER_A11Y`       |
//! | pixel       | `OVERSEER_COMPUTER_PIXEL`      |
//!
//! Helper protocol: one JSON request on stdin, one JSON response on stdout
//! (`{"ok":true,…}`); a non-zero exit or `ok:false` is an error. Captures
//! answer `{media_type, data_b64, px_w, px_h, sent_w, sent_h}`; acts answer
//! `{detail, pre_sha256, post_sha256}` when the backend can observe the
//! screen.
//!
//! Coordinate discipline: the model sends coordinates in the frame it was
//! *shown*; `scale_coords` maps them back to native pixels using the last
//! observation's frame, and the act reports both. The pre/post digest pair
//! is recorded as an audit-only `ComputerAct` event (the caller emits it;
//! see `audit_event`).
//!
//! Residual (documented, inherited from P7-1): pixel-embedded secrets
//! exfiltrating inside image bytes to vendor endpoints is accepted
//! residual — mitigated by egress-deny, the no-creds invariant, and
//! takeover suppression. OCR scanning is explicitly deferred, not silent.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use serde_json::{json, Value};

use super::{schema, ToolCtx, ToolOutput};

/// Backend-selection variables. Each names the helper binary the operator
/// points this tier at; unset (or pointing at nothing) = unconfigured.
pub const ENV_STRUCTURED: &str = "OVERSEER_COMPUTER_STRUCTURED";
pub const ENV_A11Y: &str = "OVERSEER_COMPUTER_A11Y";
pub const ENV_PIXEL: &str = "OVERSEER_COMPUTER_PIXEL";

/// Cap on `batch` members: a batch is one step, and a step is budgeted.
const MAX_BATCH: usize = 32;

/// Every action the tool accepts. `batch` wraps the rest.
const ACTIONS: &[&str] = &[
    "screenshot",
    "observe",
    "click",
    "move",
    "scroll",
    "drag",
    "hover",
    "focus",
    "type",
    "key",
    "paste",
    "submit",
    "send",
    "invoke",
];

/// Last observation's frame + digest, inside the session dir. Acts scale
/// against it (coordinate discipline) and read its digest as `pre`.
const OBS_FILE: &str = "computer-obs.json";

/// Captured pixels for the last screenshots, inside the session dir.
const CAPTURE_DIR: &str = "computer";

/// Capability tiers, best first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// Structured API (app/browser automation endpoint).
    Structured,
    /// Accessibility lookup by element name/role.
    Accessibility,
    /// Pixel act at screen coordinates.
    Pixel,
}

impl Tier {
    /// Best-first dispatch order (the tier ladder).
    pub const ORDER: [Tier; 3] = [Tier::Structured, Tier::Accessibility, Tier::Pixel];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Structured => "structured",
            Tier::Accessibility => "a11y",
            Tier::Pixel => "pixel",
        }
    }

    /// The variable that configures this tier.
    pub fn env(self) -> &'static str {
        match self {
            Tier::Structured => ENV_STRUCTURED,
            Tier::Accessibility => ENV_A11Y,
            Tier::Pixel => ENV_PIXEL,
        }
    }

    /// Rank in `ORDER` — used to report the weakest tier a batch needed.
    fn rank(self) -> usize {
        Self::ORDER.iter().position(|t| *t == self).unwrap_or(0)
    }
}

/// Configured backends. All `None` is the shipped default: computer use is
/// opt-in per tier, so a stock install cannot drive the user's screen.
#[derive(Debug, Clone, Default)]
pub struct Backends {
    pub structured: Option<PathBuf>,
    pub a11y: Option<PathBuf>,
    pub pixel: Option<PathBuf>,
}

impl Backends {
    /// Read the operator's backend config. A variable pointing at a
    /// non-existent helper counts as unset — a stale env var must not look
    /// like a working backend.
    pub fn detect() -> Self {
        let helper = |var: &str| {
            std::env::var(var)
                .ok()
                .map(|v| PathBuf::from(v.trim()))
                .filter(|p| p.is_file())
        };
        Backends {
            structured: helper(ENV_STRUCTURED),
            a11y: helper(ENV_A11Y),
            pixel: helper(ENV_PIXEL),
        }
    }

    /// The helper for `tier`, or `None` while the tier is unconfigured.
    pub fn helper(&self, tier: Tier) -> Option<&Path> {
        match tier {
            Tier::Structured => self.structured.as_deref(),
            Tier::Accessibility => self.a11y.as_deref(),
            Tier::Pixel => self.pixel.as_deref(),
        }
    }

    pub fn configured(&self, tier: Tier) -> bool {
        self.helper(tier).is_some()
    }
}

/// What serving a request needs from a tier.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Need {
    /// An explicit structured-API call (`api` handle / `invoke`).
    pub structured: bool,
    /// An element lookup by name/role.
    pub named: bool,
    /// Screen coordinates, or a scroll delta.
    pub coords: bool,
}

/// Read the tier requirements off a request.
pub fn need_of(input: &Value) -> Need {
    Need {
        structured: has_text(input, "api") || action_of(input) == "invoke",
        named: has_text(input, "name") || has_text(input, "role"),
        coords: ["x", "y", "dy"].iter().any(|k| input.get(*k).is_some()),
    }
}

/// Whether `tier` can express the request. A structured request is only
/// meaningful against a structured API (no blind downshift into a pixel
/// act); a named request prefers accessibility; coordinates and bare
/// observations are always expressible at the pixel tier — the documented
/// last resort, which reports itself in the result.
fn tier_serves(tier: Tier, need: Need) -> bool {
    match tier {
        Tier::Structured => need.structured,
        Tier::Accessibility => need.named,
        Tier::Pixel => !need.structured,
    }
}

/// Pick the first configured tier that can serve the request. Errors name
/// the missing backend(s) — the honest-unconfigured contract.
pub fn choose(need: Need, backends: &Backends) -> Result<Tier, String> {
    let mut missing: Vec<Tier> = Vec::new();
    for tier in Tier::ORDER {
        if !tier_serves(tier, need) {
            continue;
        }
        if backends.configured(tier) {
            return Ok(tier);
        }
        missing.push(tier);
    }
    if missing.is_empty() {
        return Err(
            "computer: no tier can express this request — it carries no `api`, no `name`/`role`, \
             and no coordinates"
                .into(),
        );
    }
    let tiers: Vec<String> = missing
        .iter()
        .map(|t| format!("the {} tier", t.as_str()))
        .collect();
    let vars: Vec<&str> = missing.iter().map(|t| t.env()).collect();
    Err(format!(
        "computer: unconfigured — no backend configured for {} (set {})",
        tiers.join(" or "),
        vars.join(" / ")
    ))
}

/// Map a coordinate from the frame the model was sent (`sent`) back to
/// native pixels, clamped to the frame. An unknown/zero frame is treated as
/// 1:1 — the caller reports that rather than inventing a scale factor.
pub fn scale_coords(x: f64, y: f64, native: (u32, u32), sent: (u32, u32)) -> (i64, i64) {
    let scale = |v: f64, n: u32, s: u32| -> f64 {
        if n == 0 || s == 0 {
            v
        } else {
            v * f64::from(n) / f64::from(s)
        }
    };
    let clamp = |v: f64, hi: u32| -> i64 {
        if !v.is_finite() {
            return 0;
        }
        let r = v.round() as i64;
        if hi == 0 {
            r
        } else {
            r.clamp(0, i64::from(hi) - 1)
        }
    };
    (
        clamp(scale(x, native.0, sent.0), native.0),
        clamp(scale(y, native.1, sent.1), native.1),
    )
}

/// Last observation's frame + digest — the basis for coordinate discipline
/// and the pre/post diff.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ObsState {
    pub px_w: u32,
    pub px_h: u32,
    pub sent_w: u32,
    pub sent_h: u32,
    #[serde(default)]
    pub sha256: Option<String>,
}

impl ObsState {
    fn native(&self) -> (u32, u32) {
        (self.px_w, self.px_h)
    }

    fn sent(&self) -> (u32, u32) {
        (self.sent_w, self.sent_h)
    }
}

fn action_of(input: &Value) -> String {
    input
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase()
}

fn has_text(input: &Value, key: &str) -> bool {
    input
        .get(key)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty())
}

fn cred_field(input: &Value) -> bool {
    input
        .get("cred_field")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn obs_path(ctx: &ToolCtx) -> PathBuf {
    ctx.session_dir.join(OBS_FILE)
}

fn read_obs(ctx: &ToolCtx) -> Option<ObsState> {
    std::fs::read_to_string(obs_path(ctx))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
}

fn write_obs(ctx: &ToolCtx, obs: &ObsState) {
    let path = obs_path(ctx);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(text) = serde_json::to_string(obs) {
        let _ = std::fs::write(path, text);
    }
}

/// Short content digest (`sha256:` + 16 hex chars) — enough to prove the
/// pre/post pair differs without bloating the audit log.
fn digest_short(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let hex = format!("{:x}", h.finalize());
    format!("sha256:{}", &hex[..16])
}

/// One JSON request in, one JSON response out.
fn call_helper(helper: &Path, request: &Value) -> Result<Value, String> {
    use std::io::Write;

    let mut child = Command::new(helper)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("computer: cannot start backend {}: {e}", helper.display()))?;
    match child.stdin.take() {
        Some(mut stdin) => {
            if let Err(e) = stdin.write_all(request.to_string().as_bytes()) {
                return Err(format!(
                    "computer: cannot write to backend {}: {e}",
                    helper.display()
                ));
            }
            // stdin drops here → the helper sees EOF.
        }
        None => {
            return Err(format!(
                "computer: backend {} has no stdin",
                helper.display()
            ))
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("computer: backend {} failed: {e}", helper.display()))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err = super::middle_truncate(err.trim(), 400);
        return Err(format!(
            "computer: backend {} exited with {} — {err}",
            helper.display(),
            out.status
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    let resp: Value = serde_json::from_str(stdout.trim()).map_err(|e| {
        format!(
            "computer: backend {} returned no JSON response ({e})",
            helper.display()
        )
    })?;
    if resp.get("ok").and_then(Value::as_bool) == Some(false) {
        return Err(format!(
            "computer: backend {} refused: {}",
            helper.display(),
            resp.get("error")
                .and_then(Value::as_str)
                .unwrap_or("no reason given")
        ));
    }
    Ok(resp)
}

fn copy_str(input: &Value, req: &mut Value, key: &str) {
    if let Some(v) = input.get(key).and_then(Value::as_str) {
        req[key] = Value::String(v.to_string());
    }
}

/// Capture the screen. Suppressed (credential-field focus / watch mode)
/// captures return metadata only — the backend is asked for no pixels.
fn run_capture(input: &Value, ctx: &ToolCtx, backends: &Backends) -> Result<Value, String> {
    let action = action_of(input);
    let cred = cred_field(input);
    let cfg = ctx
        .agent_config
        .as_ref()
        .map(|c| c.computer.clone())
        .unwrap_or_default();
    let suppressed = crate::computer_obs::is_suppressed(&cfg, cred, &action);
    let tier = choose(Need::default(), backends)?;
    let helper = backends
        .helper(tier)
        .ok_or_else(|| format!("computer: {} tier has no helper", tier.as_str()))?;
    let before = read_obs(ctx);

    let req = json!({
        "action": action,
        "tier": tier.as_str(),
        "capture": !suppressed,
        "cred_field": cred,
    });
    let resp = call_helper(helper, &req)?;
    let u32_of = |key: &str| resp.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
    let (px_w, px_h, sent_w, sent_h) = (
        u32_of("px_w"),
        u32_of("px_h"),
        u32_of("sent_w"),
        u32_of("sent_h"),
    );
    let media_type = resp
        .get("media_type")
        .and_then(Value::as_str)
        .unwrap_or("image/png");
    // Suppression is enforced here, not trusted to the backend: a
    // misbehaving helper that returns pixels anyway gets them dropped.
    let data = if suppressed {
        None
    } else {
        resp.get("data_b64")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
    };
    let sha = data.map(digest_short);
    write_obs(
        ctx,
        &ObsState {
            px_w,
            px_h,
            sent_w,
            sent_h,
            sha256: sha.clone(),
        },
    );

    let pre = before.and_then(|o| o.sha256);
    if data.is_none() {
        let reason = if suppressed {
            if cfg.watch_mode {
                "watch mode"
            } else {
                "credential-field focus"
            }
        } else {
            "backend returned no pixels"
        };
        return Ok(json!({
            "ok": true,
            "computer": "screenshot",
            "action": action,
            "tier": tier.as_str(),
            "suppressed": suppressed,
            "reason": reason,
            "obs": crate::computer_obs::metadata_obs(px_w, px_h, sent_w, sent_h, reason),
            "pre": pre,
            "post": Value::Null,
        }));
    }

    // Pixels are persisted next to the session (one file per capture) and
    // referenced from the result: the envelope stays small enough to never
    // spill, so the audit event always parses.
    let data = data.unwrap_or_default();
    let dir = ctx.session_dir.join(CAPTURE_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("computer: cannot create {}: {e}", dir.display()))?;
    let seq = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    let file = dir.join(format!("capture-{seq}.json"));
    let capture = json!({
        "media_type": media_type,
        "data_b64": data,
        "px_w": px_w,
        "px_h": px_h,
        "sent_w": sent_w,
        "sent_h": sent_h,
    });
    std::fs::write(&file, capture.to_string())
        .map_err(|e| format!("computer: cannot write {}: {e}", file.display()))?;
    Ok(json!({
        "ok": true,
        "computer": "screenshot",
        "action": action,
        "tier": tier.as_str(),
        "suppressed": false,
        "media_type": media_type,
        "image_file": file.display().to_string(),
        "px_w": px_w,
        "px_h": px_h,
        "sent_w": sent_w,
        "sent_h": sent_h,
        "pre": pre,
        "post": sha,
        "note": "send coordinates in the sent frame (sent_w x sent_h); they are scaled to native pixels",
    }))
}

/// One act — click/move/scroll/type/… — through the best configured tier.
fn run_act(input: &Value, ctx: &ToolCtx, backends: &Backends) -> Result<Value, String> {
    let action = action_of(input);
    let need = need_of(input);
    let tier = choose(need, backends)?;
    let helper = backends
        .helper(tier)
        .ok_or_else(|| format!("computer: {} tier has no helper", tier.as_str()))?;
    let obs = read_obs(ctx);
    let (sent, native) = obs
        .as_ref()
        .map(|o| (o.sent(), o.native()))
        .unwrap_or(((0, 0), (0, 0)));

    let mut req = json!({
        "action": action,
        "tier": tier.as_str(),
        "cred_field": cred_field(input),
        "watch_mode": ctx
            .agent_config
            .as_ref()
            .map(|c| c.computer.watch_mode)
            .unwrap_or(false),
    });
    for key in ["api", "name", "role", "text", "dy"] {
        copy_str(input, &mut req, key);
    }
    let mut scaled = Value::Null;
    if let (Some(x), Some(y)) = (
        input.get("x").and_then(Value::as_f64),
        input.get("y").and_then(Value::as_f64),
    ) {
        let (nx, ny) = scale_coords(x, y, native, sent);
        scaled = json!({"x": nx, "y": ny});
        req["x"] = json!(x);
        req["y"] = json!(y);
        req["x_native"] = json!(nx);
        req["y_native"] = json!(ny);
        req["frame"] = json!({
            "sent_w": sent.0, "sent_h": sent.1, "px_w": native.0, "px_h": native.1,
        });
    }
    let resp = call_helper(helper, &req)?;
    let pre = obs.and_then(|o| o.sha256);
    let post = resp
        .get("post_sha256")
        .and_then(Value::as_str)
        .map(str::to_string);
    let detail = resp
        .get("detail")
        .and_then(Value::as_str)
        .unwrap_or("ok")
        .to_string();
    Ok(json!({
        "ok": true,
        "computer": "act",
        "action": action,
        "tier": tier.as_str(),
        "detail": detail,
        "sent": if scaled.is_null() { Value::Null } else { json!([sent.0, sent.1]) },
        "native": scaled,
        "pre": pre,
        "post": post,
    }))
}

/// Run each member in order; the batch is only as strong as its weakest
/// member's tier, which is what the result reports.
fn run_batch(input: &Value, ctx: &ToolCtx, backends: &Backends) -> Result<Value, String> {
    let Some(actions) = input.get("actions").and_then(Value::as_array) else {
        return Err("computer: 'batch' needs an 'actions' array of actions".into());
    };
    if actions.is_empty() {
        return Err("computer: 'actions' is empty — nothing to run".into());
    }
    if actions.len() > MAX_BATCH {
        return Err(format!(
            "computer: too many actions ({} > {MAX_BATCH}) — split the batch",
            actions.len()
        ));
    }
    let pre = read_obs(ctx).and_then(|o| o.sha256);
    let mut results = Vec::new();
    let mut weakest = Tier::ORDER[0];
    for (i, a) in actions.iter().enumerate() {
        let action = action_of(a);
        if action == "batch" {
            return Err(format!(
                "computer: actions[{i}] is a nested batch — not allowed"
            ));
        }
        validate(a).map_err(|e| format!("computer: actions[{i}] — {e}"))?;
        let out = run_single(a, ctx, backends)
            .map_err(|e| format!("computer: batch failed at actions[{i}] ({action}) — {e}"))?;
        if let Some(t) = out
            .get("tier")
            .and_then(Value::as_str)
            .and_then(tier_from_str)
        {
            if t.rank() > weakest.rank() {
                weakest = t;
            }
        }
        results.push(out);
    }
    let post = read_obs(ctx).and_then(|o| o.sha256);
    Ok(json!({
        "ok": true,
        "computer": "batch",
        "count": results.len(),
        "tier": weakest.as_str(),
        "pre": pre,
        "post": post,
        "results": results,
    }))
}

fn tier_from_str(s: &str) -> Option<Tier> {
    Tier::ORDER.into_iter().find(|t| t.as_str() == s)
}

fn run_single(input: &Value, ctx: &ToolCtx, backends: &Backends) -> Result<Value, String> {
    let action = action_of(input);
    match action.as_str() {
        "screenshot" | "observe" => run_capture(input, ctx, backends),
        _ => run_act(input, ctx, backends),
    }
}

/// Validate one action request (used for top-level calls and for every
/// batch member). Returns the normalized action name.
fn validate(input: &Value) -> Result<String, String> {
    let action = action_of(input);
    if action.is_empty() {
        return Err(
            "missing 'action' — expected one of screenshot, observe, click, move, scroll, \
             type, key, paste, submit, send, invoke, batch"
                .into(),
        );
    }
    if action != "batch" && !ACTIONS.contains(&action.as_str()) {
        return Err(format!(
            "unknown action '{action}' — expected one of {}, batch",
            ACTIONS.join(", ")
        ));
    }
    match action.as_str() {
        "type" | "paste" | "key" if !has_text(input, "text") => {
            return Err(format!("'{action}' needs 'text'"));
        }
        "click" | "move"
            if input.get("x").is_none()
                && input.get("y").is_none()
                && !has_text(input, "name")
                && !has_text(input, "role") =>
        {
            return Err(format!(
                "'{action}' needs 'x'/'y' coordinates or an element 'name'/'role'"
            ));
        }
        "scroll" if input.get("dy").is_none() => {
            return Err("'scroll' needs 'dy'".into());
        }
        _ => {}
    }
    Ok(action)
}

/// Backend-injecting entry (tests, future config plumbing). `Err` is the
/// honest error string the model sees.
pub fn run_with(input: &Value, ctx: &mut ToolCtx, backends: &Backends) -> Result<Value, String> {
    let action = validate(input).map_err(|e| format!("computer: {e}"))?;
    if action == "batch" {
        return run_batch(input, ctx, backends);
    }
    run_single(input, ctx, backends)
}

/// Tool entry: detect the operator's backends, then dispatch.
pub fn run(input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
    let backends = Backends::detect();
    match run_with(input, ctx, &backends) {
        Ok(v) => ToolOutput::ok(v.to_string()),
        Err(e) => ToolOutput::err(e),
    }
}

/// Machine-readable audit record for a computer result: the tier that
/// served it plus the pre/post observation digests. `None` when the text
/// is not a computer envelope (errors, other tools, non-JSON results).
pub fn audit_event(tool_result_text: &str) -> Option<crate::event::EventKind> {
    let v: Value = serde_json::from_str(tool_result_text.trim()).ok()?;
    let kind = v.get("computer").and_then(Value::as_str)?;
    let action = match kind {
        "batch" => "batch".to_string(),
        "screenshot" => v
            .get("action")
            .and_then(Value::as_str)
            .unwrap_or("screenshot")
            .to_string(),
        _ => v.get("action").and_then(Value::as_str)?.to_string(),
    };
    Some(crate::event::EventKind::ComputerAct {
        action,
        tier: v
            .get("tier")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_string(),
        pre: v.get("pre").and_then(Value::as_str).map(str::to_string),
        post: v.get("post").and_then(Value::as_str).map(str::to_string),
        suppressed: v
            .get("suppressed")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    })
}

pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "computer".into(),
        description: concat!(
            "Drive the user's screen in tiers: a structured API call (`api`), then an element ",
            "lookup by `name`/`role`, then a pixel act at `x`/`y`. `screenshot`/`observe` capture ",
            "the screen and return the sent and native frame sizes; give coordinates in the sent ",
            "frame — they are scaled to native pixels for you. A suppressed capture (credential ",
            "field, watch mode) returns metadata only. `batch` runs up to 32 actions in order. ",
            "Backends are opt-in: an unconfigured tier errors instead of guessing."
        )
        .into(),
        input_schema: schema(
            json!({
                "action": {
                    "type": "string",
                    "description": "screenshot | observe | click | move | scroll | drag | type | key | paste | submit | send | invoke | batch"
                },
                "api": {"type": "string", "description": "Structured-API handle to invoke (structured tier)"},
                "name": {"type": "string", "description": "Element name to resolve (a11y tier)"},
                "role": {"type": "string", "description": "Element role to resolve (a11y tier)"},
                "x": {"type": "number", "description": "X in the frame you were sent"},
                "y": {"type": "number", "description": "Y in the frame you were sent"},
                "dy": {"type": "integer", "description": "Scroll delta (positive = down)"},
                "text": {"type": "string", "description": "Text to type, or key name"},
                "cred_field": {"type": "boolean", "description": "Target is a credential field (suppresses capture; escalates to identity)"},
                "actions": {"type": "array", "items": {"type": "object"}, "description": "batch: actions to run in order"},
            }),
            &["action"],
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn tmpdir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "overseer-computer-{tag}-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn ctx(dir: &std::path::Path) -> ToolCtx<'static> {
        ToolCtx {
            cwd: dir.to_path_buf(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: None,
            sandbox: false,
        }
    }

    /// Write an executable helper script and return its path. The helper
    /// protocol is the real contract — the script is the backend.
    fn helper(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// A backend that always answers the same JSON.
    fn fixed(dir: &std::path::Path, name: &str, json: &str) -> PathBuf {
        helper(dir, name, &format!("cat > /dev/null\nprintf '%s' '{json}'"))
    }

    #[test]
    fn tier_order_prefers_structured_then_a11y_then_pixel() {
        let all = Backends {
            structured: Some(PathBuf::from("/bin/true")),
            a11y: Some(PathBuf::from("/bin/true")),
            pixel: Some(PathBuf::from("/bin/true")),
        };
        let pixel_only = Backends {
            structured: None,
            a11y: None,
            pixel: all.pixel.clone(),
        };
        let a11y_and_pixel = Backends {
            structured: None,
            ..all.clone()
        };

        // A structured request: only the structured tier serves it, and it
        // is preferred over the other configured tiers.
        let api =
            json!({"action": "click", "api": "App.press", "name": "OK", "x": 10.0, "y": 20.0});
        assert_eq!(
            need_of(&api),
            Need {
                structured: true,
                named: true,
                coords: true
            }
        );
        assert_eq!(choose(need_of(&api), &all).unwrap(), Tier::Structured);
        // With the structured tier gone, the *named* half of the same
        // request is served by a11y — the result reports which tier ran.
        assert_eq!(
            choose(need_of(&api), &a11y_and_pixel).unwrap(),
            Tier::Accessibility
        );
        // A blind pixel act can never stand in for an API call: the error
        // names the tiers that could have served it.
        let err = choose(need_of(&api), &pixel_only).unwrap_err();
        assert!(err.contains("unconfigured"), "got: {err}");
        assert!(
            err.contains(ENV_STRUCTURED),
            "names the missing backend: {err}"
        );
        assert!(
            err.contains(ENV_A11Y),
            "names both served-tier misses: {err}"
        );

        // A named request: a11y beats pixel; with only pixel configured it
        // falls through to the last resort.
        let named = json!({"action": "click", "name": "OK", "role": "button"});
        assert_eq!(choose(need_of(&named), &all).unwrap(), Tier::Accessibility);
        assert_eq!(choose(need_of(&named), &pixel_only).unwrap(), Tier::Pixel);

        // A coordinate act is a pixel act: no other tier claims it.
        let coords = json!({"action": "click", "x": 1.0, "y": 2.0});
        assert_eq!(choose(need_of(&coords), &pixel_only).unwrap(), Tier::Pixel);
        let err = choose(need_of(&coords), &no_pixel()).unwrap_err();
        assert!(err.contains(ENV_PIXEL), "got: {err}");

        // Capture needs the pixel tier: the other tiers never claim it,
        // even when both are configured.
        let err = choose(Need::default(), &no_pixel()).unwrap_err();
        assert!(err.contains(ENV_PIXEL), "got: {err}");
        assert_eq!(choose(Need::default(), &pixel_only).unwrap(), Tier::Pixel);
        assert_eq!(choose(Need::default(), &all).unwrap(), Tier::Pixel);
    }

    /// Backends with everything but the pixel tier configured.
    fn no_pixel() -> Backends {
        Backends {
            structured: Some(PathBuf::from("/bin/true")),
            a11y: Some(PathBuf::from("/bin/true")),
            pixel: None,
        }
    }

    #[test]
    fn scale_coords_maps_downscaled_coords_back_exactly() {
        // A 2560x1600 framebuffer shown to the model at 1280x800: every
        // coordinate in the sent frame maps to its exact native pixel.
        let native = (2560, 1600);
        let sent = (1280, 800);
        for (x, y, nx, ny) in [
            (0.0, 0.0, 0, 0),
            (640.0, 400.0, 1280, 800),
            (1.0, 1.0, 2, 2),
            (1279.0, 799.0, 2558, 1598),
        ] {
            assert_eq!(scale_coords(x, y, native, sent), (nx, ny), "({x},{y})");
        }
        // Identity when the frames match (no scaling configured).
        assert_eq!(scale_coords(42.0, 7.0, sent, sent), (42, 7));
        // Unknown frame → 1:1, and out-of-frame coordinates clamp inside.
        assert_eq!(scale_coords(42.0, 7.0, (0, 0), (0, 0)), (42, 7));
        assert_eq!(scale_coords(9999.0, -5.0, sent, sent), (1279, 0));
    }

    #[test]
    fn unconfigured_backends_error_names_the_missing_backend() {
        let dir = tmpdir("unconfigured");
        let mut c = ctx(&dir);
        let none = Backends::default();
        let err = run_with(
            &json!({"action": "click", "x": 1.0, "y": 2.0}),
            &mut c,
            &none,
        )
        .unwrap_err();
        assert!(err.contains("unconfigured"), "got: {err}");
        assert!(err.contains(ENV_PIXEL), "names the pixel backend: {err}");
        let err = run_with(&json!({"action": "screenshot"}), &mut c, &none).unwrap_err();
        assert!(err.contains(ENV_PIXEL), "got: {err}");
        // Through the registry: the tool is advertised and dispatched, and
        // an unconfigured backend is an honest error, not a fake success.
        assert!(super::super::TOOL_NAMES.contains(&"computer"));
        let mut reg = super::super::ToolRegistry::core(crate::perm::Policy::allow_all());
        let out = reg.call("computer", &json!({"action": "screenshot"}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains("unconfigured"), "got: {}", out.text);
        // Unknown actions are refused with the action list.
        let bad = run_with(&json!({"action": "frobnicate"}), &mut c, &none).unwrap_err();
        assert!(bad.contains("unknown action"), "got: {bad}");
    }

    #[test]
    fn batch_runs_each_action_in_order_and_reports_the_weakest_tier() {
        let dir = tmpdir("batch");
        let log = dir.join("calls.log");
        // The helper appends each request to a log so the order is
        // observable, then answers with a post digest.
        let pixel = helper(
            &dir,
            "pixel.sh",
            &format!(
                "{{ cat; printf '\\n'; }} >> {log}\nprintf '%s' '{{\"ok\":true,\"detail\":\"done\",\"post_sha256\":\"sha256:post\"}}'",
                log = log.display()
            ),
        );
        let backends = Backends {
            structured: None,
            a11y: None,
            pixel: Some(pixel),
        };
        let mut c = ctx(&dir);
        let out = run_with(
            &json!({"action": "batch", "actions": [
                {"action": "click", "x": 10.0, "y": 10.0},
                {"action": "type", "text": "hello"},
                {"action": "scroll", "dy": -3},
            ]}),
            &mut c,
            &backends,
        )
        .unwrap();
        assert_eq!(out["computer"], "batch");
        assert_eq!(out["count"], 3);
        assert_eq!(out["tier"], "pixel");
        assert_eq!(out["results"].as_array().unwrap().len(), 3);
        let calls = std::fs::read_to_string(&log).unwrap();
        let actions: Vec<String> = calls
            .lines()
            .map(|l| {
                serde_json::from_str::<Value>(l).unwrap()["action"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect();
        assert_eq!(actions, vec!["click", "type", "scroll"], "order preserved");
        // A failing member stops the batch and names its index.
        let err = run_with(
            &json!({"action": "batch", "actions": [{"action": "click", "x": 1.0, "y": 1.0}, {"action": "nope"}]}),
            &mut c,
            &backends,
        )
        .unwrap_err();
        assert!(err.contains("actions[1]"), "got: {err}");
        // Empty / oversized batches are refused before any backend runs.
        let empty = run_with(
            &json!({"action": "batch", "actions": []}),
            &mut c,
            &backends,
        );
        assert!(empty.unwrap_err().contains("empty"));
    }

    #[test]
    fn capture_persists_pixels_and_records_the_pre_post_diff() {
        let dir = tmpdir("capture");
        let pixel = fixed(
            &dir,
            "pixel.sh",
            r#"{"ok":true,"media_type":"image/png","data_b64":"aGVsbG8=","px_w":2560,"px_h":1600,"sent_w":1280,"sent_h":800}"#,
        );
        let backends = Backends {
            structured: None,
            a11y: None,
            pixel: Some(pixel),
        };
        let mut c = ctx(&dir);
        let out = run_with(&json!({"action": "screenshot"}), &mut c, &backends).unwrap();
        assert_eq!(out["tier"], "pixel");
        assert_eq!(out["px_w"], 2560);
        assert_eq!(out["sent_w"], 1280);
        assert!(out["post"].as_str().unwrap().starts_with("sha256:"));
        // Pixels are persisted, referenced — not inlined (no spill).
        let file = out["image_file"].as_str().unwrap().to_string();
        let saved: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(saved["data_b64"], "aGVsbG8=");
        // The frame is remembered for the next act's coordinate scaling.
        let obs = read_obs(&c).expect("observation state");
        assert_eq!((obs.px_w, obs.sent_w), (2560, 1280));
        // The audit record carries the tier and the pre/post digest pair.
        let ev = audit_event(&out.to_string()).expect("audit event");
        match ev {
            crate::event::EventKind::ComputerAct {
                tier,
                post,
                suppressed,
                ..
            } => {
                assert_eq!(tier, "pixel");
                assert!(post.is_some());
                assert!(!suppressed);
            }
            other => panic!("expected ComputerAct, got {other:?}"),
        }
    }

    #[test]
    fn takeover_suppression_returns_metadata_only() {
        let dir = tmpdir("suppress");
        // The backend is told not to capture; it answers with dimensions
        // only — and this helper would fail the test if pixels were sent.
        let pixel = fixed(
            &dir,
            "pixel.sh",
            r#"{"ok":true,"px_w":2560,"px_h":1600,"sent_w":1280,"sent_h":800}"#,
        );
        let backends = Backends {
            structured: None,
            a11y: None,
            pixel: Some(pixel),
        };
        let mut c = ctx(&dir);
        c.agent_config = Some(crate::agent::AgentConfig::default());
        let out = run_with(
            &json!({"action": "screenshot", "cred_field": true}),
            &mut c,
            &backends,
        )
        .unwrap();
        assert_eq!(out["suppressed"], true);
        let obs = out["obs"].as_str().unwrap();
        assert!(obs.contains("capture suppressed"), "got: {obs}");
        assert!(obs.contains("2560x1600"));
        assert!(!obs.contains("data_b64"), "no pixel bytes: {obs}");
        assert!(out.get("image_file").is_none(), "no capture file written");
    }

    #[test]
    fn backend_failures_are_reported_not_swallowed() {
        let dir = tmpdir("failure");
        // Non-zero exit.
        let failing = helper(&dir, "fail.sh", "echo 'no display' >&2\nexit 3");
        let backends = Backends {
            structured: None,
            a11y: None,
            pixel: Some(failing),
        };
        let mut c = ctx(&dir);
        let err = run_with(&json!({"action": "screenshot"}), &mut c, &backends).unwrap_err();
        assert!(err.contains("exited with"), "got: {err}");
        assert!(err.contains("no display"), "stderr tail: {err}");
        // ok:false.
        let refusing = fixed(
            &dir,
            "refuse.sh",
            r#"{"ok":false,"error":"permission denied"}"#,
        );
        let backends = Backends {
            structured: None,
            a11y: None,
            pixel: Some(refusing),
        };
        let err = run_with(&json!({"action": "screenshot"}), &mut c, &backends).unwrap_err();
        assert!(err.contains("permission denied"), "got: {err}");
    }

    #[test]
    fn acts_scale_coordinates_against_the_last_observation() {
        let dir = tmpdir("act-scale");
        let log = dir.join("calls.log");
        let pixel = helper(
            &dir,
            "pixel.sh",
            &format!(
                "{{ cat; printf '\\n'; }} >> {log}\nprintf '%s' '{{\"ok\":true,\"post_sha256\":\"sha256:post\"}}'",
                log = log.display()
            ),
        );
        let backends = Backends {
            structured: None,
            a11y: None,
            pixel: Some(pixel.clone()),
        };
        let mut c = ctx(&dir);
        // Pretend the model was shown a 1280x800 downscale of a 2560x1600
        // screen; a click at (640,400) must land at native (1280,800).
        write_obs(
            &c,
            &ObsState {
                px_w: 2560,
                px_h: 1600,
                sent_w: 1280,
                sent_h: 800,
                sha256: Some("sha256:before".into()),
            },
        );
        let out = run_with(
            &json!({"action": "click", "x": 640.0, "y": 400.0}),
            &mut c,
            &backends,
        )
        .unwrap();
        assert_eq!(out["native"], json!({"x": 1280, "y": 800}));
        assert_eq!(out["pre"], "sha256:before");
        let call: Value = serde_json::from_str(
            std::fs::read_to_string(&log)
                .unwrap()
                .lines()
                .next()
                .unwrap(),
        )
        .unwrap();
        assert_eq!(call["x_native"], 1280);
        assert_eq!(call["y_native"], 800);
        assert_eq!(call["frame"]["sent_w"], 1280);
        assert_eq!(call["frame"]["px_w"], 2560);
    }

    #[test]
    fn order_union_segment_present_boundary_ok() {
        // P7-3 ORDER-union: the `computer` segment rides the frozen union
        // order and the static/dynamic boundary still holds.
        let mut cfg = crate::agent::AgentConfig::default();
        let segs = crate::prompt::assemble(&cfg);
        let computer = segs
            .iter()
            .find(|s| s.name == "computer")
            .expect("computer segment present");
        assert!(computer.cacheable);
        assert!(crate::prompt::boundary_ok(&segs));
        for s in &segs {
            if s.cacheable {
                assert!(
                    crate::prompt::ORDER.contains(&s.name),
                    "static section '{}' must be in the union ORDER",
                    s.name
                );
            }
        }
        // An ablated arm is absent-skipped (no advertisement for an arm
        // the model cannot call).
        cfg.disabled_tools.push("computer".into());
        assert!(crate::prompt::assemble(&cfg)
            .iter()
            .all(|s| s.name != "computer"));
    }
}
