//! Tool system (playbook Ch.2 §2.4, Ch.4 §2).
//!
//! Small resident core — earn-your-tokens rule: read (line numbers,
//! offset/limit), edit (anchored search/replace, uniqueness, read-before-edit),
//! write, bash (stateless exec), grep, glob. Every result passes the byte
//! budget: ~30K chars inline, middle-truncate, beyond that spill-to-file with
//! {path, preview, size} so the model can re-read on demand.
//! Error messages are prompts: name the invariant violated, suggest the repair.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub mod bash;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod read;
pub mod write;

/// Hard cap on inline tool results (proven default: Claude Code's ~30K).
pub const INLINE_CAP: usize = 30_000;
/// Spill threshold: results over this go to a file instead of context.
pub const SPILL_THRESHOLD: usize = 30_000;

/// Per-invocation context passed to tools.
pub struct ToolCtx {
    pub cwd: PathBuf,
    /// Session dir for spilled tool output (…/tool-outputs/).
    pub session_dir: PathBuf,
    /// Monotonic counter for naming spilled files.
    pub spill_seq: u64,
}

/// What a tool produced. `text` is what enters context.
pub struct ToolOutput {
    pub text: String,
    pub is_error: bool,
    /// Bytes of the raw result before truncation/spill.
    pub raw_bytes: u64,
    pub spilled_to: Option<String>,
}

impl ToolOutput {
    pub fn ok(text: String) -> Self {
        let raw = text.len() as u64;
        ToolOutput {
            text,
            is_error: false,
            raw_bytes: raw,
            spilled_to: None,
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
        }
    }
}

/// The resident tool registry + per-session tool state (e.g. the read-before-
/// edit tracker — a harness-enforced anti-hallucination invariant, Ch.4 §2.3).
pub struct ToolRegistry {
    pub specs: Vec<crate::provider::ToolSpec>,
    /// Paths the agent has read this session (canonicalized).
    read_paths: HashSet<PathBuf>,
}

impl ToolRegistry {
    pub fn core() -> Self {
        ToolRegistry {
            specs: vec![
                bash::spec(),
                read::spec(),
                write::spec(),
                edit::spec(),
                grep::spec(),
                glob::spec(),
            ],
            read_paths: HashSet::new(),
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

    /// Dispatch a tool call. Never panics: unknown names and bad inputs become
    /// error ToolOutputs that teach the model the contract.
    pub fn call(&mut self, name: &str, input: &Value, ctx: &mut ToolCtx) -> ToolOutput {
        let out = match name {
            "bash" => bash::run(input, ctx),
            "read" => read::run(input, ctx, self),
            "write" => write::run(input, ctx, self),
            "edit" => edit::run(input, ctx, self),
            "grep" => grep::run(input, ctx),
            "glob" => glob::run(input, ctx),
            other => ToolOutput::err(format!(
                "Unknown tool '{other}'. Available tools: bash, read, write, edit, grep, glob."
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
