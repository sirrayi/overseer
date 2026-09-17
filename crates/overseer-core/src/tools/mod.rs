//! Tool system (playbook Ch.2 §2.4, Ch.4 §2).
//!
//! Small resident core — earn-your-tokens rule: read (line numbers,
//! offset/limit), edit (anchored search/replace, uniqueness, read-before-edit),
//! write, bash (stateless exec), grep, glob. Every result passes the byte
//! budget: ~30K chars inline, middle-truncate, beyond that spill-to-file with
//! {path, preview, size} so the model can re-read on demand.
//! Error messages are prompts: name the invariant violated, suggest the repair.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

pub mod bash;
pub mod computer;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod plan;
pub mod read;
pub mod repomap;
pub mod skill;
pub mod task;
pub mod write;

/// Hard cap on inline tool results (proven default: Claude Code's ~30K).
pub const INLINE_CAP: usize = 30_000;
/// Spill threshold: results over this go to a file instead of context.
pub const SPILL_THRESHOLD: usize = 30_000;

/// Per-invocation context passed to tools.
pub struct ToolCtx<'a> {
    pub cwd: PathBuf,
    /// Session dir for spilled tool output (…/tool-outputs/).
    pub session_dir: PathBuf,
    /// Monotonic counter for naming spilled files.
    pub spill_seq: u64,
    /// Provider handle for the `task` subagent tool; None in contexts with
    /// no provider (tests, dry runs) → `task` fails with an honest error.
    /// `Arc` so background subagents can own a handle across threads.
    pub provider: Option<std::sync::Arc<dyn crate::provider::Provider>>,
    /// Parent agent config for subagent inheritance (model, cwd, budgets).
    pub agent_config: Option<crate::agent::AgentConfig>,
    /// Subagent spawn counter for session-dir naming.
    pub subagent_seq: u64,
    /// Active checkpoint for this user prompt (P1.9): write/edit snapshot
    /// files here before touching them. None = checkpointing off.
    pub checkpoint: Option<&'a mut Checkpoint>,
    /// P1.5: wrap bash calls in the platform sandbox when one exists.
    pub sandbox: bool,
}

/// A per-user-prompt checkpoint (P1.9): `dir` holds file snapshots +
/// `manifest.jsonl`; `done` is the canonical-path set already captured,
/// so each file is snapshotted once — before its first write.
pub struct Checkpoint {
    pub dir: PathBuf,
    pub done: HashSet<PathBuf>,
}

/// Snapshot `path` into the active checkpoint before a write/edit touches
/// it. First-touch only per checkpoint; files that don't exist yet are
/// recorded with `existed: false` so rewind deletes them. Best-effort —
/// a snapshot failure never blocks the write itself.
pub fn snapshot(ctx: &mut ToolCtx, path: &Path) {
    let Some(cp) = ctx.checkpoint.as_deref_mut() else {
        return;
    };
    // Manifest paths must be absolute — rewind resolves them from a
    // different cwd. Canonicalize handles existing files (and symlinks);
    // for not-yet-created files (the `existed: false` case, where
    // canonicalize fails) anchor relative paths at the tool cwd first.
    let anchored = if path.is_absolute() {
        path.to_path_buf()
    } else {
        ctx.cwd.join(path)
    };
    // components() drops `.` and duplicate separators, so `a.txt` and
    // `./a.txt` dedup to one manifest entry.
    let anchored: PathBuf = anchored.components().collect();
    let key = anchored.canonicalize().unwrap_or(anchored);
    if !cp.done.insert(key.clone()) {
        return;
    }
    let stored = key
        .to_string_lossy()
        .replace('/', "%2F")
        .replace('\\', "%5C");
    let files_dir = cp.dir.join("files");
    let _ = std::fs::create_dir_all(&files_dir);
    let existed = key.exists();
    if existed {
        let _ = std::fs::copy(&key, files_dir.join(&stored));
    }
    let line = json!({
        "path": key.to_string_lossy(),
        "stored": stored,
        "existed": existed,
    });
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(cp.dir.join("manifest.jsonl"))
    {
        use std::io::Write;
        let _ = writeln!(f, "{line}");
    }
}

/// What a tool produced. `text` is what enters context.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    /// Bytes of the raw result before truncation/spill.
    pub raw_bytes: u64,
    pub spilled_to: Option<String>,
    /// True when the permission gate denied the call (it never ran).
    pub denied: bool,
}

impl ToolOutput {
    pub fn ok(text: String) -> Self {
        let raw = text.len() as u64;
        ToolOutput {
            text,
            is_error: false,
            raw_bytes: raw,
            spilled_to: None,
            denied: false,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        let text = msg.into();
        let raw = text.len() as u64;
        ToolOutput {
            text,
            is_error: true,
            raw_bytes: raw,
            spilled_to: None,
            denied: false,
        }
    }
    /// The gate denied the call — never executed, auditable via `denied`.
    pub fn denied(reason: String) -> Self {
        let mut o = Self::err(format!("Permission denied: {reason}"));
        o.denied = true;
        o
    }
}

/// One completed read: the file's mtime at read time + the line range
/// returned. Dedup key is (path, mtime, range) per playbook Ch.6 §2.3.
struct ReadRecord {
    mtime: Option<SystemTime>,
    start: usize,
    end: usize,
}

/// The resident tool registry + per-session tool state (e.g. the read-before-
/// edit tracker — a harness-enforced anti-hallucination invariant, Ch.4 §2.3).
/// Owns the permission policy: the gate lives at the dispatch boundary so no
/// caller path can skip it (Invariant 3).
/// All tool names the core registry can emit — the validation set for
/// `--no-tools` ablations (typo'd names fail fast, not silently no-op).
pub const TOOL_NAMES: [&str; 12] = [
    "bash", "read", "write", "edit", "grep", "glob", "plan", "task", "skill", "repo_map", "symbol",
    "computer",
];

pub struct ToolRegistry {
    pub specs: Vec<crate::provider::ToolSpec>,
    /// Paths the agent has read this session (canonicalized).
    read_paths: HashSet<PathBuf>,
    /// Read history for dedup: canonical path → (mtime, line range) list.
    read_log: HashMap<PathBuf, Vec<ReadRecord>>,
    pub policy: crate::perm::Policy,
    /// Rule-of-Two latch notices (P3.10): drained by the agent loop and
    /// emitted as `Tainted` events.
    pub taint_notices: Vec<String>,
    /// P4.3 ablation: names removed via --no-tools. Hidden from the spec
    /// list AND refused at dispatch — defense in depth.
    disabled: HashSet<String>,
}

impl ToolRegistry {
    pub fn core(policy: crate::perm::Policy) -> Self {
        // Sorted by name — the tool list serializes deterministically
        // regardless of registration order (Invariant 2: stable prefix).
        let mut specs = vec![
            bash::spec(),
            read::spec(),
            write::spec(),
            edit::spec(),
            grep::spec(),
            glob::spec(),
            plan::spec(),
            task::spec(),
            skill::spec(),
            repomap::spec_map(),
            repomap::spec_symbol(),
            computer::spec(),
        ];
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        ToolRegistry {
            specs,
            read_paths: HashSet::new(),
            read_log: HashMap::new(),
            policy,
            taint_notices: Vec::new(),
            disabled: HashSet::new(),
        }
    }

    /// Read-only registry for quarantined subagents (playbook Ch.3 §9.6):
    /// writes stay single-threaded in the parent agent. No `task` either —
    /// subagents cannot spawn subagents.
    pub fn readonly(policy: crate::perm::Policy) -> Self {
        ToolRegistry {
            specs: vec![read::spec(), grep::spec(), glob::spec()],
            read_paths: HashSet::new(),
            read_log: HashMap::new(),
            policy,
            taint_notices: Vec::new(),
            disabled: HashSet::new(),
        }
    }

    /// Plan-mode registry (P1.4 capability removal): mutating tools aren't
    /// merely denied — they're absent from the spec list, so the model
    /// cannot call them at all. Read tools + the plan artifact tool.
    pub fn plan_mode(policy: crate::perm::Policy) -> Self {
        let mut specs = vec![read::spec(), grep::spec(), glob::spec(), plan::spec()];
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        ToolRegistry {
            specs,
            read_paths: HashSet::new(),
            read_log: HashMap::new(),
            policy,
            taint_notices: Vec::new(),
            disabled: HashSet::new(),
        }
    }

    /// P4.3 ablation: drop `names` from the advertised spec list and refuse
    /// them at dispatch. Unknown names are ignored here — the CLI validates
    /// against TOOL_NAMES before this is ever called.
    pub fn disable(&mut self, names: &[String]) {
        for n in names {
            self.disabled.insert(n.clone());
        }
        self.specs.retain(|s| !self.disabled.contains(&s.name));
    }

    pub fn mark_read(&mut self, path: &Path) {
        if let Ok(p) = path.canonicalize() {
            self.read_paths.insert(p);
        }
    }

    pub fn was_read(&self, path: &Path) -> bool {
        path.canonicalize()
            .map(|p| self.read_paths.contains(&p))
            .unwrap_or(false)
    }

    /// P1.3 read dedup: true when `path` was already read with the same
    /// `mtime` and a range covering `[start, end)` — the identical content
    /// is already in context, so the caller returns a stub instead.
    pub fn dedup_hit(
        &self,
        path: &Path,
        mtime: Option<SystemTime>,
        start: usize,
        end: usize,
    ) -> bool {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.read_log
            .get(&key)
            .map(|recs| {
                recs.iter()
                    .any(|r| r.mtime == mtime && r.start <= start && end <= r.end)
            })
            .unwrap_or(false)
    }

    /// True when `path` has read history but every record's mtime differs —
    /// the file changed since the last read (the "diff" half of the rule).
    pub fn mtime_changed(&self, path: &Path, mtime: Option<SystemTime>) -> bool {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.read_log
            .get(&key)
            .map(|recs| !recs.is_empty() && recs.iter().all(|r| r.mtime != mtime))
            .unwrap_or(false)
    }

    /// Record a completed read for dedup (and the read-before-edit tracker).
    pub fn record_read(
        &mut self,
        path: &Path,
        mtime: Option<SystemTime>,
        start: usize,
        end: usize,
    ) {
        let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        self.read_log
            .entry(key)
            .or_default()
            .push(ReadRecord { mtime, start, end });
        self.mark_read(path);
    }

    /// The permission policy (live mode badges read the preset from here).
    pub fn policy(&self) -> &crate::perm::Policy {
        &self.policy
    }

    /// Dispatch a tool call. Disabled tools are refused before the
    /// permission gate so an ablated run never prompts a human for a tool
    /// that can never execute. The gate runs next — a `Gate::Deny`
    /// (rule-denied, human-denied, or headless Ask) returns a denied
    /// ToolOutput; the call never runs. Never panics: unknown names and bad
    /// inputs become error ToolOutputs that teach the model the contract.
    pub fn call(&mut self, name: &str, input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
        if self.disabled.contains(name) {
            return ToolOutput::err(format!(
                "Tool '{name}' is disabled for this run (--no-tools)."
            ));
        }
        match self.policy.gate(name, input) {
            crate::perm::Gate::Allow => {}
            crate::perm::Gate::Deny(reason) => return ToolOutput::denied(reason),
        }
        // B1-2 (Instructor/FastMCP): hand-rolled required/type check against
        // the advertised `input_schema` before dispatch. Zero-dep (serde_json
        // is already in the tree); failures return a field-level error that
        // names the violated field — no dispatch, no side effects.
        if let Some(spec) = self.specs.iter().find(|s| s.name == name) {
            if let Err(e) = check_args(&spec.input_schema, input) {
                return e;
            }
        }
        let out = match name {
            "bash" => bash::run(input, ctx),
            "read" => read::run(input, ctx, self),
            "write" => write::run(input, ctx, self),
            "edit" => edit::run(input, ctx, self),
            "grep" => grep::run(input, ctx),
            "glob" => glob::run(input, ctx),
            "plan" => plan::run(input, ctx),
            "task" => task::run(input, ctx),
            "skill" => skill::run(input, ctx),
            "repo_map" => repomap::run_map(input, ctx),
            "symbol" => repomap::run_symbol(input, ctx),
            "computer" => computer::run(input, ctx),
            other => ToolOutput::err(format!(
                "Unknown tool '{other}'. Available tools: bash, read, write, edit, grep, glob, plan, task, skill, repo_map, symbol, computer."
            )),
        };
        // Rule-of-Two bookkeeping (P3.10): this result may carry untrusted
        // content or secret material — latch before the next call is gated.
        if let Some(notice) = self.policy.note_result(name, input, &out.text) {
            self.taint_notices.push(notice);
        }
        enforce_budget(out, ctx)
    }
}

/// Enforce the tool-result byte budget (playbook Ch.6 §2.2):
/// ≤30K chars inline; larger results spill to a file and return a pointer.
pub fn enforce_budget(out: ToolOutput, ctx: &mut ToolCtx) -> ToolOutput {
    if out.text.len() <= INLINE_CAP {
        return out;
    }
    // B1-3: TOON tabular pass — uniform JSON arrays shrink ~30-60% before
    // the spill decision. JSON stays at API boundaries; only the inline
    // text the model reads changes.
    let out = ToolOutput {
        text: crate::toon::maybe_encode_json_array(&out.text),
        ..out
    };
    if out.text.len() <= INLINE_CAP {
        return out;
    }
    let dir = ctx.session_dir.join("tool-outputs");
    let _ = std::fs::create_dir_all(&dir);
    ctx.spill_seq += 1;
    let path = dir.join(format!("output-{}.txt", ctx.spill_seq));
    let raw = out.text.clone();
    let preview: String = raw.chars().take(4_000).collect();
    let size = raw.len();
    match std::fs::write(&path, &raw) {
        Ok(()) => ToolOutput {
            text: format!(
                "Output too large ({size} bytes) — written to {}.\n\
                 Preview:\n{preview}\n[...truncated...]\n\
                 Use `read` with offset/limit or `grep` on that file for more.",
                path.display()
            ),
            is_error: out.is_error,
            raw_bytes: out.raw_bytes.max(size as u64),
            spilled_to: Some(path.display().to_string()),
            denied: out.denied,
        },
        Err(e) => {
            // Fall back to middle-truncation if the spill write failed.
            let head: String = raw.chars().take(INLINE_CAP / 2).collect();
            let tail: String = raw
                .chars()
                .skip(raw.chars().count().saturating_sub(INLINE_CAP / 2))
                .collect();
            ToolOutput {
                text: format!("{head}\n[...middle-truncated after spill failed: {e}...]\n{tail}"),
                is_error: true,
                raw_bytes: out.raw_bytes.max(size as u64),
                spilled_to: None,
                denied: out.denied,
            }
        }
    }
}

/// Provenance wrap (P3.10): tool output enters the model context inside
/// explicit markers so injected instructions can't pose as user/system
/// text. Applied at the IR layer (Block::ToolResult.content) — the event
/// log stores raw output and rehydrate re-wraps, so live and resumed
/// views stay byte-identical.
pub fn provenance_wrap(name: &str, text: &str) -> String {
    format!("<tool_result tool=\"{name}\">\n{text}\n</tool_result>")
}

/// Middle-truncate a string to `cap` chars, keeping head and tail
/// (errors cluster at the tail, context at the head — Ch.6 §2.2).
pub fn middle_truncate(s: &str, cap: usize) -> String {
    let n = s.chars().count();
    if n <= cap {
        return s.to_string();
    }
    let half = cap / 2;
    let head: String = s.chars().take(half).collect();
    let tail: String = s.chars().skip(n - half).collect();
    format!("{head}\n[...{} chars truncated...]\n{tail}", n - cap)
}

/// Validate `input` against a tool's advertised JSON schema (B1-2).
/// Hand-rolled over the subset `schema()` emits: object `properties` with
/// `type` in {string, integer, boolean, array, object}, `required`, and
/// `additionalProperties: false`. Unknown/complex subschemas pass through —
/// this is a typo-catcher, not a validator; per-tool `run()` stays
/// authoritative. Zero new deps.
pub fn check_args(schema: &Value, input: &Value) -> Result<(), ToolOutput> {
    let obj = match input.as_object() {
        Some(o) => o,
        None => {
            return Err(ToolOutput::err(
                "Tool input must be a JSON object matching the tool's input_schema.",
            ))
        }
    };
    let props = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .cloned()
        .unwrap_or_default();
    let required: Vec<String> = schema
        .get("required")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    for key in &required {
        if !obj.contains_key(key) {
            return Err(ToolOutput::err(format!(
                "Missing required parameter '{key}' — check the tool's input_schema."
            )));
        }
    }
    let no_extra = schema
        .get("additionalProperties")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    if !no_extra {
        for key in obj.keys() {
            if !props.contains_key(key) {
                return Err(ToolOutput::err(format!(
                    "Unknown parameter '{key}' — check the tool's input_schema."
                )));
            }
        }
    }
    for (key, val) in obj {
        let Some(decl) = props.get(key) else { continue };
        let Some(want) = decl.get("type").and_then(|v| v.as_str()) else {
            continue;
        };
        let ok = match want {
            "string" => val.is_string(),
            "integer" => val.is_i64() || val.is_u64(),
            "boolean" => val.is_boolean(),
            "array" => val.is_array(),
            "object" => val.is_object(),
            _ => true, // unknown type word: pass through
        };
        if !ok {
            return Err(ToolOutput::err(format!(
                "Parameter '{key}' must be {want} — check the tool's input_schema."
            )));
        }
    }
    Ok(())
}

/// Helper for arg extraction with an error that teaches the schema.
pub fn need_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolOutput> {
    input
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ToolOutput::err(format!("Missing required string parameter '{key}'.")))
}

pub fn opt_u64(input: &Value, key: &str) -> Option<u64> {
    input.get(key).and_then(Value::as_u64)
}

/// Resolve a user-supplied path against the session cwd.
pub fn resolve(ctx: &ToolCtx, path: &str) -> PathBuf {
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        ctx.cwd.join(p)
    }
}

/// Shared JSON-schema fragment builders.
pub fn schema(properties: Value, required: &[&str]) -> Value {
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir() -> PathBuf {
        let d = std::env::temp_dir().join(format!("overseer-test-{}", uuid::Uuid::now_v7()));
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
            subagent_seq: 0,
            checkpoint: None,
            sandbox: false,
        }
    }

    #[test]
    fn check_args_rejects_missing_required_without_dispatch() {
        // B1-2: malformed args fail at the schema check — the tool never runs.
        let dir = tmpdir();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);
        let out = reg.call("read", &serde_json::json!({}), &mut c);
        assert!(out.is_error, "missing required path must error");
        assert!(
            out.text.contains("Missing required parameter 'path'"),
            "field-level error, got: {}",
            out.text
        );
    }

    #[test]
    fn check_args_rejects_wrong_type_without_dispatch() {
        let dir = tmpdir();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);
        let out = reg.call("read", &serde_json::json!({"path": 42}), &mut c);
        assert!(out.is_error);
        assert!(
            out.text.contains("must be string"),
            "type error names the kind, got: {}",
            out.text
        );
    }

    #[test]
    fn check_args_rejects_unknown_param() {
        let dir = tmpdir();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);
        let out = reg.call(
            "read",
            &serde_json::json!({"path": "a.txt", "bogus": true}),
            &mut c,
        );
        assert!(out.is_error);
        assert!(
            out.text.contains("Unknown parameter 'bogus'"),
            "got: {}",
            out.text
        );
    }

    #[test]
    fn all_core_specs_deny_additional_properties() {
        // B1-2 FastMCP audit: every advertised spec must be strict.
        let reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        for spec in &reg.specs {
            assert_eq!(
                spec.input_schema.get("additionalProperties"),
                Some(&serde_json::Value::Bool(false)),
                "spec {} must set additionalProperties:false",
                spec.name
            );
        }
        assert_eq!(reg.specs.len(), TOOL_NAMES.len());
    }

    #[test]
    fn read_dedup_returns_stub_for_unchanged_reread() {
        let dir = tmpdir();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);

        let first = reg.call("read", &json!({"path": "a.txt"}), &mut c);
        assert!(first.text.contains("one"));
        // Identical re-read → stub, not the bytes again.
        let second = reg.call("read", &json!({"path": "a.txt"}), &mut c);
        assert!(second.text.contains("[unchanged]"));
        assert!(!second.text.contains("two"));
        // A covered sub-range is also deduped.
        let sub = reg.call(
            "read",
            &json!({"path": "a.txt", "offset": 2, "limit": 1}),
            &mut c,
        );
        assert!(sub.text.contains("[unchanged]"));

        // Modify the file → fresh content with a change note.
        std::thread::sleep(std::time::Duration::from_millis(5));
        std::fs::write(dir.join("a.txt"), "one\nTWO\nthree\nfour\n").unwrap();
        let third = reg.call("read", &json!({"path": "a.txt"}), &mut c);
        assert!(third
            .text
            .contains("[file modified since your previous read]"));
        assert!(third.text.contains("TWO"));
    }

    #[test]
    fn plan_persists_checklist_to_session_dir() {
        let dir = tmpdir();
        let mut reg = ToolRegistry::core(crate::perm::Policy::headless(dir.clone()));
        let mut c = ctx(&dir);

        let out = reg.call(
            "plan",
            &json!({"items": [
                {"content": "explore", "status": "completed"},
                {"content": "implement", "status": "in_progress"},
                {"content": "verify", "status": "pending"}
            ]}),
            &mut c,
        );
        assert!(!out.is_error);
        assert!(out.text.contains("1 in progress"));

        // Persisted + resumable: the artifact lives in the session dir.
        let md = std::fs::read_to_string(c.session_dir.join("plan.md")).unwrap();
        assert!(md.contains("- [x] explore"));
        assert!(md.contains("- [~] implement"));
        assert!(md.contains("- [ ] verify"));
        assert!(c.session_dir.join("plan.json").exists());

        // Bad status is rejected with a repair hint.
        let bad = reg.call(
            "plan",
            &json!({"items": [{"content": "x", "status": "done"}]}),
            &mut c,
        );
        assert!(bad.is_error);
        assert!(bad.text.contains("pending|in_progress|completed"));
    }

    #[test]
    fn edit_returns_hunk_and_validates_json() {
        let dir = tmpdir();
        let file = (1..=20)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(dir.join("b.txt"), &file).unwrap();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);

        reg.call("read", &json!({"path": "b.txt"}), &mut c);
        let out = reg.call(
            "edit",
            &json!({"path": "b.txt", "old_string": "line10", "new_string": "LINE10"}),
            &mut c,
        );
        assert!(!out.is_error);
        // Hunk = the change + context, never the whole file.
        assert!(out.text.contains("LINE10"));
        assert!(out.text.contains("line9"));
        assert!(out.text.contains("line11"));
        assert!(!out.text.contains("line1\n"));
        assert!(!out.text.contains("line20"));

        // Apply-time validation: a JSON-corrupting edit is refused,
        // file untouched.
        std::fs::write(dir.join("c.json"), "{\"a\": 1}").unwrap();
        reg.call("read", &json!({"path": "c.json"}), &mut c);
        let bad = reg.call(
            "edit",
            &json!({"path": "c.json", "old_string": "1", "new_string": "1,"}),
            &mut c,
        );
        assert!(bad.is_error);
        assert!(bad.text.contains("invalid json"), "got: {}", bad.text);
        assert_eq!(
            std::fs::read_to_string(dir.join("c.json")).unwrap(),
            "{\"a\": 1}"
        );
    }

    #[test]
    fn disable_hides_and_refuses_tool() {
        let dir = tmpdir();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        reg.disable(&["grep".to_string(), "task".to_string()]);
        // Spec list no longer advertises them (capability removal).
        let names: Vec<&str> = reg.specs.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"grep"));
        assert!(!names.contains(&"task"));
        assert!(names.contains(&"read"));
        // And dispatch refuses them outright (defense in depth).
        let mut c = ctx(&dir);
        let out = reg.call("grep", &json!({"pattern": "x"}), &mut c);
        assert!(out.is_error);
        assert!(out.text.contains("disabled"));
    }

    #[test]
    fn checkpoint_snapshots_before_first_write() {
        let dir = tmpdir();
        std::fs::write(dir.join("old.txt"), "original").unwrap();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut cp = Checkpoint {
            dir: dir.join("cp/e5"),
            done: HashSet::new(),
        };
        let mut c = ToolCtx {
            cwd: dir.clone(),
            session_dir: dir.join("session"),
            spill_seq: 0,
            provider: None,
            agent_config: None,
            subagent_seq: 0,
            checkpoint: Some(&mut cp),
            sandbox: false,
        };

        reg.call(
            "write",
            &json!({"path": "old.txt", "content": "v1"}),
            &mut c,
        );
        reg.call(
            "write",
            &json!({"path": "old.txt", "content": "v2"}),
            &mut c,
        );
        reg.call(
            "write",
            &json!({"path": "new.txt", "content": "fresh"}),
            &mut c,
        );

        // One manifest entry per file — the second write didn't re-snapshot.
        let manifest = std::fs::read_to_string(dir.join("cp/e5/manifest.jsonl")).unwrap();
        assert_eq!(manifest.lines().count(), 2);
        let entries: Vec<Value> = manifest
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let old_e = &entries[0];
        assert_eq!(old_e["existed"], true);
        // The snapshot holds the PRE-write content.
        let snap = std::fs::read_to_string(
            dir.join("cp/e5/files")
                .join(old_e["stored"].as_str().unwrap()),
        )
        .unwrap();
        assert_eq!(snap, "original");
        // A created file is recorded as not existing → rewind deletes it.
        assert_eq!(entries[1]["existed"], false);
        // Regression (live-hammer find): manifest paths must be absolute
        // even for not-yet-created files — canonicalize can't resolve a
        // nonexistent path, so a raw relative path would make rewind
        // delete nothing.
        let rec = std::path::Path::new(entries[1]["path"].as_str().unwrap());
        assert!(rec.is_absolute());
        assert_eq!(
            rec.canonicalize().unwrap(),
            dir.join("new.txt").canonicalize().unwrap()
        );
    }
}
