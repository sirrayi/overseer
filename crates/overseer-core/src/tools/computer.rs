//! computer tool (P7-3, S5) — computer-use dispatch.
//!
//! Backend order (D1): a **cua-driver** binary — `OVERSEER_COMPUTER_DRIVER`
//! or `cua-driver` on PATH — serves every action over its `mcp` stdio
//! server (see `cua.rs` for transport and response shapes). When no driver
//! is configured the legacy helper tiers run the subset they can express:
//! a **structured API** (app/browser automation endpoint) is cheaper and
//! more deterministic than an **accessibility** lookup by element
//! name/role, which beats a blind **pixel** act at screen coordinates.
//! `choose()` walks that order and takes the first tier the operator has
//! configured. A structured request never silently degrades into a pixel
//! act — if the tier that can express it is unconfigured the call fails
//! honestly. Either way the advertised spec is identical: the model never
//! knows which backend serves it.
//!
//! The engine links no OS framework: every backend is an opt-in process
//! named by an environment variable or found on PATH (the platform
//! bindings live there, not here). A missing backend is an `unconfigured`
//! error naming the variable to set — never a fake success, never a quiet
//! downgrade.
//!
//! | backend     | variable                       |
//! |-------------|--------------------------------|
//! | cua-driver  | `OVERSEER_COMPUTER_DRIVER`     |
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

mod cua;
mod shape;

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
/// The cua-driver binary (a path) — when configured it serves every action
/// and the helper tiers below are not consulted (D1).
pub const ENV_DRIVER: &str = "OVERSEER_COMPUTER_DRIVER";

/// Cap on `batch` members: a batch is one step, and a step is budgeted.
const MAX_BATCH: usize = 32;

/// Every action the tool accepts (S5 vocabulary). `batch` wraps the rest.
const ACTIONS: &[&str] = &[
    "apps",
    "windows",
    "launch",
    "observe",
    "screenshot",
    "zoom",
    "click",
    "type",
    "key",
    "set",
    "scroll",
    "drag",
    "menu",
    "verify",
    "browser",
    "browser_click",
    "browser_type",
    "navigate",
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
/// opt-in per backend, so a stock install cannot drive the user's screen.
#[derive(Debug, Clone, Default)]
pub struct Backends {
    /// cua-driver binary — `OVERSEER_COMPUTER_DRIVER` or `cua-driver` on
    /// PATH. When set it serves EVERY action (D1); the helper tiers below
    /// only run while no driver exists.
    pub driver: Option<PathBuf>,
    // DEFERRED(owner): remove the helper-binary protocol once cua-driver is
    // the only backend in use
    pub structured: Option<PathBuf>,
    pub a11y: Option<PathBuf>,
    pub pixel: Option<PathBuf>,
}

/// An env-var driver path must be absolute: a relative one resolves
/// against the agent's cwd, where a workspace file could pose as the
/// driver and get spawned unsandboxed (F1). The surviving path is
/// canonicalized so `..`/symlinks collapse to the real file.
fn driver_env_path(raw: Option<String>) -> Option<PathBuf> {
    let p = PathBuf::from(raw?.trim());
    if !p.is_absolute() || !p.is_file() {
        return None;
    }
    p.canonicalize().ok()
}

impl Backends {
    /// Read the operator's backend config. A variable pointing at a
    /// non-existent helper counts as unset — a stale env var must not look
    /// like a working backend. The driver probe is a PATH stat, never a
    /// spawn (D1).
    pub fn detect() -> Self {
        let helper = |var: &str| {
            std::env::var(var)
                .ok()
                .map(|v| PathBuf::from(v.trim()))
                .filter(|p| p.is_file())
        };
        Backends {
            driver: driver_env_path(std::env::var(ENV_DRIVER).ok())
                .or_else(|| super::struct_search::find_on_path(&["cua-driver"])),
            structured: helper(ENV_STRUCTURED),
            a11y: helper(ENV_A11Y),
            pixel: helper(ENV_PIXEL),
        }
    }

    /// Whether anything can serve the tool — a driver, or any helper tier.
    pub fn any_configured(&self) -> bool {
        self.driver.is_some() || Tier::ORDER.iter().any(|t| self.configured(*t))
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

/// Owner-only file write (0600 unix, best-effort elsewhere) for capture
/// artifacts — screen pixels routinely contain secrets.
fn write_owner_only(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(content.as_bytes())
            })
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
    }
}

/// Parent-env keys a helper inherits: the bash allowlist's basics plus the
/// three tier variables (a helper may chain to a sibling tier).
fn helper_env_allowed(k: &str) -> bool {
    matches!(
        k,
        "PATH"
            | "HOME"
            | "USER"
            | "SHELL"
            | "TERM"
            | "LANG"
            | "LC_ALL"
            | "TMPDIR"
            | ENV_STRUCTURED
            | ENV_A11Y
            | ENV_PIXEL
            | "CI"
    ) || k.starts_with("LC_")
}

/// One JSON request in, one JSON response out.
fn call_helper(helper: &Path, request: &Value) -> Result<Value, String> {
    use std::io::Write;

    // F1 (extreme): no-creds invariant — helpers inherit the allowlisted
    // env only (mirrors bash.rs). The full process env carries brokered
    // secrets and webhook tokens; ComputerConfig::env_allowed was dead code.
    let mut cmd = Command::new(helper);
    cmd.env_clear()
        .envs(super::filter_env(std::env::vars_os(), helper_env_allowed))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = super::spawn_retrying_busy(&mut cmd)
        .map_err(|e| format!("computer: cannot start backend {}: {e}", helper.display()))?;
    match child.stdin.take() {
        Some(mut stdin) => {
            // A helper that exits without reading its request closes the
            // pipe; its exit status is the real answer, so EPIPE is not.
            if let Err(e) = stdin
                .write_all(request.to_string().as_bytes())
                .or_else(|e| match e.kind() {
                    std::io::ErrorKind::BrokenPipe => Ok(()),
                    _ => Err(e),
                })
            {
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
    let suppressed = crate::computer_obs::is_suppressed(&cfg, cred);
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
    // Red-team capture fix: screen pixels routinely contain secrets — the
    // dir is owner-only (0700) like spill dirs (unix; best-effort elsewhere).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // UUID-v7 names are unique and time-ordered; a count of the dir would
    // reuse a live index after any deletion.
    let file = dir.join(format!("capture-{}.json", uuid::Uuid::now_v7()));
    let capture = json!({
        "media_type": media_type,
        "data_b64": data,
        "px_w": px_w,
        "px_h": px_h,
        "sent_w": sent_w,
        "sent_h": sent_h,
    });
    write_owner_only(&file, &capture.to_string())
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
        "note": "send coordinates in the sent frame (sent_w x sent_h); they are scaled to native pixels (image not persisted across resume)",
    }))
}

// DEFERRED(owner): remove the helper-binary protocol once cua-driver is
// the only backend in use
/// One act — click/scroll/type/… — through the best configured tier. This
/// is the legacy path; under a cua-driver backend `cua::run` serves every
/// action instead.
fn run_act(input: &Value, ctx: &ToolCtx, backends: &Backends) -> Result<Value, String> {
    let action = action_of(input);
    // Element targeting is driver vocabulary — the helper protocol has no
    // element handles.
    if input.get("element").is_some() {
        return Err(format!(
            "computer: '{action}' element targeting needs the cua-driver backend — pass x/y, \
             or set OVERSEER_COMPUTER_DRIVER"
        ));
    }
    // The helper protocol's scroll is vertical only (dy).
    if action == "scroll" && input.get("dy").is_none() {
        return Err(
            "computer: horizontal scroll (dx) needs the cua-driver backend — set \
             OVERSEER_COMPUTER_DRIVER"
                .into(),
        );
    }
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
    // 'key' takes `keys` in the S5 vocabulary; the helper protocol's key
    // act reads `text`.
    if action == "key" {
        copy_str(input, &mut req, "keys");
        if let Some(keys) = input.get("keys").and_then(Value::as_str) {
            req["text"] = json!(keys);
        }
    }
    // Named targeting is helper vocabulary: api/name/role ride along
    // verbatim so the structured/a11y tiers can resolve the act (F4).
    for key in ["text", "button", "api", "name", "role"] {
        copy_str(input, &mut req, key);
    }
    for key in ["dy", "dx", "to_x", "to_y", "count"] {
        if let Some(v) = input.get(key) {
            req[key] = v.clone();
        }
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
/// member's tier, which is what the result reports. Under a driver the
/// member envelopes already say `"tier":"cua"`.
fn run_batch(input: &Value, ctx: &ToolCtx, st: &mut ComputerState) -> Result<Value, String> {
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
        // A capture's pixels can't ride a batch result envelope.
        if matches!(action.as_str(), "screenshot" | "zoom") {
            return Err(format!(
                "computer: actions[{i}] is a capture — captures can't be batched — call \
                 screenshot on its own"
            ));
        }
        validate(a).map_err(|e| format!("computer: actions[{i}] — {e}"))?;
        let out = run_single(a, ctx, st)
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
    let tier = if st.backends.driver.is_some() {
        "cua"
    } else {
        weakest.as_str()
    };
    Ok(json!({
        "ok": true,
        "computer": "batch",
        "count": results.len(),
        "tier": tier,
        "pre": pre,
        "post": post,
        "results": results,
    }))
}

fn tier_from_str(s: &str) -> Option<Tier> {
    Tier::ORDER.into_iter().find(|t| t.as_str() == s)
}

fn run_single(input: &Value, ctx: &ToolCtx, st: &mut ComputerState) -> Result<Value, String> {
    if st.backends.driver.is_some() {
        return cua::run(input, ctx, st);
    }
    legacy_single(input, ctx, &st.backends)
}

// DEFERRED(owner): remove the helper-binary protocol once cua-driver is
// the only backend in use
/// The helper-binary path — the fallback when no cua-driver is configured
/// (D1). It serves the subset of the S5 vocabulary it can express and
/// rejects driver-only actions with a pointer at the fix.
fn legacy_single(input: &Value, ctx: &ToolCtx, backends: &Backends) -> Result<Value, String> {
    let action = action_of(input);
    match action.as_str() {
        "screenshot" => run_capture(input, ctx, backends),
        "click" | "type" | "key" | "scroll" | "drag" => run_act(input, ctx, backends),
        "observe" => Err(
            "computer: 'observe' needs the cua-driver backend (the helper protocol cannot list \
             elements) — use 'screenshot', or set OVERSEER_COMPUTER_DRIVER"
                .into(),
        ),
        other => Err(format!(
            "computer: '{other}' needs the cua-driver backend — install cua-driver or set \
             OVERSEER_COMPUTER_DRIVER (the helper protocol cannot serve it)"
        )),
    }
}

/// Validate one action request (used for top-level calls and for every
/// batch member). Checks are backend-agnostic — backend-specific params
/// (pid, window_id, element…) are enforced at dispatch, so the legacy
/// helpers keep serving the actions they can express. Returns the
/// normalized action name.
fn validate(input: &Value) -> Result<String, String> {
    let action = action_of(input);
    if action.is_empty() {
        return Err(format!(
            "missing 'action' — expected one of {}, batch",
            ACTIONS.join(", ")
        ));
    }
    if action != "batch" && !ACTIONS.contains(&action.as_str()) {
        return Err(format!(
            "unknown action '{action}' — expected one of {}, batch",
            ACTIONS.join(", ")
        ));
    }
    let pair = |a: &str, b: &str| input.get(a).is_some() && input.get(b).is_some();
    match action.as_str() {
        "launch" if !has_text(input, "app") => {
            return Err("'launch' needs 'app' (an application name)".into());
        }
        // pid/window_id are driver params — the legacy helpers work without
        // them, so they are enforced in cua.rs, not here.
        "zoom" if !(pair("x1", "y1") && pair("x2", "y2")) => {
            return Err("'zoom' needs the region 'x1','y1','x2','y2'".into());
        }
        // api/name/role are helper-vocabulary targeting — under a driver
        // cua::click refuses them with a pointer at element/x,y (F4).
        "click"
            if input.get("element").is_none()
                && !pair("x", "y")
                && input.get("api").is_none()
                && input.get("name").is_none()
                && input.get("role").is_none() =>
        {
            return Err(
                "'click' needs an 'element' index, 'x'/'y' coordinates, or a name/role".into(),
            );
        }
        "type" if !has_text(input, "text") => {
            return Err("'type' needs 'text'".into());
        }
        "key" if !has_text(input, "keys") => {
            return Err("'key' needs 'keys' (\"cmd+s\" or \"return\")".into());
        }
        "set" if input.get("element").is_none() || input.get("value").is_none() => {
            return Err("'set' needs 'element' and 'value'".into());
        }
        "scroll" if input.get("dx").is_none() && input.get("dy").is_none() => {
            return Err("'scroll' needs 'dx' or 'dy' (the scroll direction)".into());
        }
        "drag" if !(pair("x", "y") && pair("to_x", "to_y")) => {
            return Err("'drag' needs 'x','y' and 'to_x','to_y'".into());
        }
        "menu"
            if input
                .get("path")
                .and_then(Value::as_array)
                .is_none_or(|p| p.is_empty()) =>
        {
            return Err("'menu' needs 'path' — an array of menu item names".into());
        }
        "verify" if input.get("expect").is_none() => {
            return Err("'verify' needs 'expect' (a predicate object or list)".into());
        }
        "browser" if !pair("pid", "window_id") && !has_text(input, "tab") => {
            return Err(
                "'browser' needs 'pid'+'window_id' (to bind) or 'tab' (to snapshot)".into(),
            );
        }
        "browser_click" | "browser_type" | "navigate" if !has_text(input, "tab") => {
            return Err(format!("'{action}' needs 'tab' (a bound browser tab)"));
        }
        "browser_click" if input.get("ref").is_none() && !pair("x", "y") => {
            return Err("'browser_click' needs a 'ref' or 'x'/'y'".into());
        }
        "browser_type" if !has_text(input, "text") => {
            return Err("'browser_type' needs 'text'".into());
        }
        "navigate" if !has_text(input, "url") => {
            return Err("'navigate' needs 'url'".into());
        }
        _ => {}
    }
    // F3: coordinate type check — presence is not enough. A string/bool/null
    // coordinate would otherwise scale to Null and dispatch a broken act.
    for key in ["x", "y", "dx", "dy", "to_x", "to_y", "x1", "y1", "x2", "y2"] {
        if let Some(v) = input.get(key) {
            let ok = v.as_f64().is_some_and(|f| f.is_finite());
            if !ok {
                return Err(format!(
                    "'{action}' coordinate '{key}' must be a finite number"
                ));
            }
        }
    }
    Ok(action)
}

/// Computer-tool state held by the registry: the backends detected once at
/// registry build (D1) and, once spawned, the live cua-driver client (D2).
pub struct ComputerState {
    pub backends: Backends,
    pub(crate) driver: Option<cua::Live>,
}

impl ComputerState {
    /// Probe the operator's backends (env vars + PATH stat, no spawns).
    pub fn detect() -> Self {
        ComputerState {
            backends: Backends::detect(),
            driver: None,
        }
    }

    /// Fixed-environment constructor for tests and future config plumbing.
    pub fn new(backends: Backends) -> Self {
        ComputerState {
            backends,
            driver: None,
        }
    }
}

impl Drop for ComputerState {
    /// `end_session` on registry drop is best-effort (D2): the client's own
    /// Drop kills the child either way.
    fn drop(&mut self) {
        if let Some(mut live) = self.driver.take() {
            live.end_session();
        }
    }
}

/// Backend-injecting entry (tests, future config plumbing). `Err` is the
/// honest error string the model sees.
pub fn run_with(input: &Value, ctx: &mut ToolCtx, st: &mut ComputerState) -> Result<Value, String> {
    let action = validate(input).map_err(|e| format!("computer: {e}"))?;
    if action == "batch" {
        return run_batch(input, ctx, st);
    }
    run_single(input, ctx, st)
}

/// Tool entry: dispatch through the registry's computer state.
pub fn run(input: &Value, ctx: &mut ToolCtx, st: &mut ComputerState) -> ToolOutput {
    match run_with(input, ctx, st) {
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

/// The image block a `computer` capture carries (S5): the envelope's
/// `image_file` is read back into a `Block::Image` the agent appends after
/// the tool result — pixels ride the message, never the event log. All
/// provider adapters already serialize `Block::Image`.
pub fn image_block(tool_result_text: &str) -> Option<crate::ir::Block> {
    let v: Value = serde_json::from_str(tool_result_text.trim()).ok()?;
    let file = v.get("image_file").and_then(Value::as_str)?;
    let cap: Value = serde_json::from_str(&std::fs::read_to_string(file).ok()?).ok()?;
    Some(crate::ir::Block::Image {
        media_type: cap.get("media_type")?.as_str()?.to_string(),
        data_b64: cap.get("data_b64")?.as_str()?.to_string(),
        px_w: cap.get("px_w")?.as_u64()? as u32,
        px_h: cap.get("px_h")?.as_u64()? as u32,
        sent_w: cap.get("sent_w")?.as_u64()? as u32,
        sent_h: cap.get("sent_h")?.as_u64()? as u32,
    })
}

/// The advertised spec is the same with or without a backend (D1 — spec
/// byte-stability is Invariant 2's): one description line per action
/// family, no duplicated enum prose.
pub fn spec() -> crate::provider::ToolSpec {
    crate::provider::ToolSpec {
        name: "computer".into(),
        description: concat!(
            "Drive the user's screen and Chrome/Edge tabs. Tiered: the structured API first, then ",
            "element lookups by name/role, then pixel acts. 'apps'/'windows' find targets; ",
            "'observe' lists elements (index as 'element'); 'screenshot'/'zoom' image it; ",
            "'click'/'type'/'key'/'set'/'scroll'/'drag'/'menu'/'launch' act; 'verify' checks ",
            "predicates; 'browser*'/'navigate' drive tabs via 'tab'/'ref'; 'batch' runs ≤32 actions. ",
            "x/y are pixels of the image you saw, passed verbatim in the driver's frame; after ",
            "'zoom' they are crop pixels the driver maps back (from_zoom). Nothing is screenshotted ",
            "implicitly; a suppressed capture returns metadata only; credential fields are never ",
            "typed into."
        )
        .into(),
        input_schema: schema(
            json!({
                "action": {
                    "type": "string",
                    "description": "apps | windows | launch | observe | screenshot | zoom | click | type | key | set | scroll | drag | menu | verify | browser | browser_click | browser_type | navigate | batch"
                },
                "pid": {"type": "integer", "description": "target app"},
                "window_id": {"type": "integer", "description": "target window"},
                "app": {"type": "string", "description": "name to launch"},
                "query": {"type": "string"},
                "limit": {"type": "integer", "description": "observe cap (150)"},
                "max": {"type": "integer"},
                "element": {"type": "integer", "description": "index from observe"},
                "x": {"type": "number"}, "y": {"type": "number"},
                "to_x": {"type": "number"}, "to_y": {"type": "number"},
                "dx": {"type": "number"}, "dy": {"type": "number"},
                "x1": {"type": "number"}, "y1": {"type": "number"},
                "x2": {"type": "number"}, "y2": {"type": "number"},
                "button": {"type": "string", "description": "left|right"},
                "count": {"type": "integer", "description": "1|2"},
                "text": {"type": "string"},
                "keys": {"type": "string", "description": "\"cmd+s\"|\"return\""},
                "value": {},
                "path": {"type": "array", "items": {"type": "string"}},
                "expect": {},
                "tab": {"type": "string", "description": "bound browser tab"},
                "ref": {"type": "string", "description": "browser element ref"},
                "url": {"type": "string"},
                "cred_field": {"type": "boolean", "description": "target is a credential field"},
                "actions": {"type": "array", "items": {"type": "object"}},
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
            broker: None,
        }
    }

    /// A registry-free state pinned at these (legacy) backends — driver
    /// never detected in tests.
    fn st(b: &Backends) -> ComputerState {
        ComputerState::new(b.clone())
    }

    #[test]
    fn driver_env_path_requires_an_absolute_path() {
        // F1: a relative env path resolves against the agent's cwd — a
        // workspace `./cua-driver` would be spawned unsandboxed.
        let dir = tmpdir("driverenv");
        std::fs::write(dir.join("cua-driver"), "#!/bin/sh\n").unwrap();
        // Canonicalize the expectation: driver_env_path canonicalizes
        // (no `..`/symlinks), and on macOS the temp dir is /var →
        // /private/var.
        let real = dir.join("cua-driver").canonicalize().unwrap();
        let rel = std::path::Path::new(&real)
            .strip_prefix(std::env::current_dir().unwrap())
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| PathBuf::from("./cua-driver"));
        assert_eq!(
            driver_env_path(Some(rel.display().to_string())),
            None,
            "relative path must be refused"
        );
        assert_eq!(driver_env_path(Some("./cua-driver".into())), None);
        assert_eq!(driver_env_path(Some("cua-driver".into())), None);
        assert_eq!(driver_env_path(None), None);
        // Absolute paths survive — canonicalized (no `..`).
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let dotted = format!("{}/sub/../cua-driver", dir.display());
        assert_eq!(driver_env_path(Some(dotted)), Some(real.clone()));
        assert_eq!(
            driver_env_path(Some(format!("  {}  ", real.display()))),
            Some(real)
        );
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
            driver: None,
            structured: Some(PathBuf::from("/bin/true")),
            a11y: Some(PathBuf::from("/bin/true")),
            pixel: Some(PathBuf::from("/bin/true")),
        };
        let pixel_only = Backends {
            driver: None,
            structured: None,
            a11y: None,
            pixel: all.pixel.clone(),
        };
        let a11y_and_pixel = Backends {
            driver: None,
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
            driver: None,
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
            &mut st(&none),
        )
        .unwrap_err();
        assert!(err.contains("unconfigured"), "got: {err}");
        assert!(err.contains(ENV_PIXEL), "names the pixel backend: {err}");
        let err = run_with(&json!({"action": "screenshot"}), &mut c, &mut st(&none)).unwrap_err();
        assert!(err.contains(ENV_PIXEL), "got: {err}");
        // Through the registry: with no helper the tool is not advertised,
        // and a call is an honest error naming how to configure it.
        assert!(super::super::TOOL_NAMES.contains(&"computer"));
        let mut reg = super::super::ToolRegistry::core_with(
            crate::perm::Policy::allow_all(),
            super::super::Optional::default(),
        );
        assert!(!reg.specs.iter().any(|s| s.name == "computer"));
        let out = reg.call("computer", &json!({"action": "screenshot"}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains(ENV_PIXEL), "got: {}", out.text);
        // Unknown actions are refused with the action list.
        let bad = run_with(&json!({"action": "frobnicate"}), &mut c, &mut st(&none)).unwrap_err();
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
            driver: None,
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
            &mut st(&backends),
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
            &mut st(&backends),
        )
        .unwrap_err();
        assert!(err.contains("actions[1]"), "got: {err}");
        // Captures can't be batched — their pixels would be dropped.
        let err = run_with(
            &json!({"action": "batch", "actions": [{"action": "screenshot"}]}),
            &mut c,
            &mut st(&backends),
        )
        .unwrap_err();
        assert!(err.contains("captures can't be batched"), "got: {err}");
        // Empty / oversized batches are refused before any backend runs.
        let empty = run_with(
            &json!({"action": "batch", "actions": []}),
            &mut c,
            &mut st(&backends),
        );
        assert!(empty.unwrap_err().contains("empty"));
    }

    #[test]
    fn named_act_forwards_api_name_and_role_to_the_helper() {
        // F4: api/name/role are helper vocabulary — they ride the request
        // verbatim so a11y/structured tiers can resolve named acts.
        let dir = tmpdir("named");
        let log = dir.join("req.log");
        let a11y = helper(
            &dir,
            "a11y.sh",
            &format!(
                "cat >> {log}\nprintf '%s' '{{\"ok\":true,\"detail\":\"done\"}}'",
                log = log.display()
            ),
        );
        let backends = Backends {
            driver: None,
            structured: None,
            a11y: Some(a11y),
            pixel: None,
        };
        let mut c = ctx(&dir);
        run_with(
            &json!({"action": "click", "api": "App.press", "name": "OK", "role": "button"}),
            &mut c,
            &mut st(&backends),
        )
        .unwrap();
        let req: Value =
            serde_json::from_str(std::fs::read_to_string(&log).unwrap().trim()).unwrap();
        assert_eq!(req["api"], "App.press");
        assert_eq!(req["name"], "OK");
        assert_eq!(req["role"], "button");
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
            driver: None,
            structured: None,
            a11y: None,
            pixel: Some(pixel),
        };
        let mut c = ctx(&dir);
        let out = run_with(&json!({"action": "screenshot"}), &mut c, &mut st(&backends)).unwrap();
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
            driver: None,
            structured: None,
            a11y: None,
            pixel: Some(pixel),
        };
        let mut c = ctx(&dir);
        c.agent_config = Some(crate::agent::AgentConfig::default());
        let out = run_with(
            &json!({"action": "screenshot", "cred_field": true}),
            &mut c,
            &mut st(&backends),
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
            driver: None,
            structured: None,
            a11y: None,
            pixel: Some(failing),
        };
        let mut c = ctx(&dir);
        let err =
            run_with(&json!({"action": "screenshot"}), &mut c, &mut st(&backends)).unwrap_err();
        assert!(err.contains("exited with"), "got: {err}");
        assert!(err.contains("no display"), "stderr tail: {err}");
        // ok:false.
        let refusing = fixed(
            &dir,
            "refuse.sh",
            r#"{"ok":false,"error":"permission denied"}"#,
        );
        let backends = Backends {
            driver: None,
            structured: None,
            a11y: None,
            pixel: Some(refusing),
        };
        let err =
            run_with(&json!({"action": "screenshot"}), &mut c, &mut st(&backends)).unwrap_err();
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
            driver: None,
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
            &mut st(&backends),
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
    fn guidance_rides_the_spec_not_the_static_prompt() {
        // The computer tool is deferred, so its guidance lives in its own
        // description (seen via `tools` op=search), never the static prefix.
        let mut cfg = crate::agent::AgentConfig::default();
        let segs = crate::prompt::assemble(&cfg);
        assert!(segs.iter().all(|s| s.name != "computer"));
        let desc = spec().description;
        for needle in ["structured API", "from_zoom", "credential"] {
            assert!(desc.contains(needle), "{needle} missing: {desc}");
        }
        assert!(!desc.contains("scaled for you"), "{desc}");
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
        // An ablated arm stays absent too.
        cfg.disabled_tools.push("computer".into());
        assert!(crate::prompt::assemble(&cfg)
            .iter()
            .all(|s| s.name != "computer"));
    }

    #[test]
    fn string_coordinates_are_refused() {
        // F3: presence-only validation dispatched coordinate-less acts.
        let dir = std::env::temp_dir().join(format!("overseer-cu-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = ctx(&dir);
        let backends = Backends::default();
        let err = run_with(
            &serde_json::json!({"action": "click", "x": "abc", "y": "def"}),
            &mut ctx,
            &mut st(&backends),
        )
        .expect_err("string coords must refuse");
        assert!(err.contains("finite number"), "got: {err}");
    }

    #[test]
    fn helper_env_forwards_every_tier_variable_and_nothing_undefined() {
        for k in [ENV_STRUCTURED, ENV_A11Y, ENV_PIXEL, "PATH", "LC_CTYPE"] {
            assert!(helper_env_allowed(k), "{k} must reach the helper");
        }
        for k in [
            "OVERSEER_COMPUTER_API",
            "FOO_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
        ] {
            assert!(!helper_env_allowed(k), "{k} must not reach the helper");
        }
    }

    #[test]
    fn a_deleted_capture_never_lets_the_next_one_overwrite_a_survivor() {
        let dir = tmpdir("capture-seq");
        let pixel = fixed(
            &dir,
            "pixel.sh",
            r#"{"ok":true,"media_type":"image/png","data_b64":"aGVsbG8=","px_w":2,"px_h":2,"sent_w":2,"sent_h":2}"#,
        );
        let backends = Backends {
            driver: None,
            structured: None,
            a11y: None,
            pixel: Some(pixel),
        };
        let mut c = ctx(&dir);
        let shot = |c: &mut ToolCtx| {
            run_with(&json!({"action": "screenshot"}), c, &mut st(&backends)).unwrap()["image_file"]
                .as_str()
                .unwrap()
                .to_string()
        };
        let first = shot(&mut c);
        let second = shot(&mut c);
        std::fs::remove_file(&first).unwrap();
        std::fs::write(&second, "survivor").unwrap();
        let third = shot(&mut c);
        assert_ne!(third, second, "a new capture reused a live file name");
        assert_eq!(std::fs::read_to_string(&second).unwrap(), "survivor");
    }

    /// Child-process probe with a non-UTF-8 env value (see bash's twin).
    #[cfg(unix)]
    #[test]
    fn a_non_utf8_env_value_does_not_kill_the_helper_call() {
        use std::os::unix::ffi::OsStringExt;
        const MARK: &str = "LC_OVERSEER_T12_PROBE";
        if std::env::var_os(MARK).is_some() {
            let dir = tmpdir("nonutf8");
            let h = fixed(&dir, "h.sh", r#"{"ok":true}"#);
            let out = call_helper(&h, &json!({"probe": 1})).unwrap();
            assert_eq!(out["ok"], true);
            return;
        }
        let out = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::computer::tests::a_non_utf8_env_value_does_not_kill_the_helper_call",
                "--test-threads=1",
            ])
            .env(MARK, std::ffi::OsString::from_vec(vec![b'f', 0xff]))
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(out.status.success(), "{stdout}");
        assert!(stdout.contains("1 passed"), "{stdout}");
    }

    #[test]
    fn helper_env_carries_no_secrets() {
        // F1: helpers inherit the allowlisted env only (mirrors bash.rs).
        // Spawns a real dump-env helper and asserts the secret is absent.
        // NOTE: call_helper speaks JSON-RPC (writes request, parses JSON
        // reply) so the helper must print a JSON object, not raw env.
        std::env::set_var("OVERSEER_STRESS_SECRET_KEY", "leak-value-123");
        let helper = std::env::temp_dir().join(format!("dump-env-{}", uuid::Uuid::now_v7()));
        std::fs::write(
            &helper,
            "#!/bin/sh\nread _line\nif env | grep -q OVERSEER_STRESS_SECRET_KEY; then echo '{\"ok\":false}'; else echo '{\"ok\":true}'; fi\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755));
        }
        let out = call_helper(&helper, &serde_json::json!({"probe": 1})).expect("helper runs");
        std::env::remove_var("OVERSEER_STRESS_SECRET_KEY");
        assert_eq!(out["ok"], true, "child env must not carry secrets: {out}");
        let _ = std::fs::remove_file(&helper);
    }
}
