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
pub mod edit;
pub mod glob;
pub mod grep;
pub mod plan;
pub mod read;
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
    pub provider: Option<&'a dyn crate::provider::Provider>,
    /// Parent agent config for subagent inheritance (model, cwd, budgets).
    pub agent_config: Option<crate::agent::AgentConfig>,
    /// Subagent spawn counter for session-dir naming.
    pub subagent_seq: u64,
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
pub struct ToolRegistry {
    pub specs: Vec<crate::provider::ToolSpec>,
    /// Paths the agent has read this session (canonicalized).
    read_paths: HashSet<PathBuf>,
    /// Read history for dedup: canonical path → (mtime, line range) list.
    read_log: HashMap<PathBuf, Vec<ReadRecord>>,
    policy: crate::perm::Policy,
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
        ];
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        ToolRegistry {
            specs,
            read_paths: HashSet::new(),
            read_log: HashMap::new(),
            policy,
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
        }
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

    /// Dispatch a tool call. The permission gate runs first — Deny and
    /// (headless) Ask both return a denied ToolOutput; the call never runs.
    /// Never panics: unknown names and bad inputs become error ToolOutputs
    /// that teach the model the contract.
    pub fn call(&mut self, name: &str, input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
        match self.policy.check(name, input) {
            crate::perm::Verdict::Allow => {}
            crate::perm::Verdict::Ask { reason } | crate::perm::Verdict::Deny { reason } => {
                return ToolOutput::denied(reason);
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
            other => ToolOutput::err(format!(
                "Unknown tool '{other}'. Available tools: bash, read, write, edit, grep, glob, plan, task."
            )),
        };
        enforce_budget(out, ctx)
    }
}

/// Enforce the tool-result byte budget (playbook Ch.6 §2.2):
/// ≤30K chars inline; larger results spill to a file and return a pointer.
pub fn enforce_budget(out: ToolOutput, ctx: &mut ToolCtx) -> ToolOutput {
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
        }
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
        assert!(bad.text.contains("invalid JSON"));
        assert_eq!(
            std::fs::read_to_string(dir.join("c.json")).unwrap(),
            "{\"a\": 1}"
        );
    }
}
