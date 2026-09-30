//! Tool system (playbook Ch.2 §2.4, Ch.4 §2).
//!
//! Small resident core — earn-your-tokens rule: read (line numbers,
//! offset/limit), edit (anchored search/replace, uniqueness, read-before-edit),
//! write, bash (stateless exec), grep, glob. Every result passes the byte
//! budget: ~30K chars inline, middle-truncate, beyond that spill-to-file with
//! {path, preview, size} so the model can re-read on demand.
//! Error messages are prompts: name the invariant violated, suggest the repair.
//!
//! Optional tools (`computer`, `struct_search`, `diagnostics`) are
//! advertised only when this host can serve them. [`Optional::detect`] runs
//! once, when [`ToolRegistry::core`] builds the registry — env/PATH lookups
//! and file-existence checks only, no process spawns — so the spec array is
//! fixed and byte-stable for the session (Invariant 2). An absent optional
//! tool is also refused at dispatch with the reason and how to enable it.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::{json, Value};

pub mod bash;
pub mod computer;
pub mod diagnostics;
pub mod edit;
pub mod glob;
pub mod grep;
pub mod mcp_tool;
pub mod memory_tool;
pub mod plan;
pub mod read;
pub mod repomap;
#[cfg(feature = "code-mode")]
pub mod run_code;
pub mod skill;
pub mod struct_search;
pub mod task;
pub mod tools_tool;
pub mod write;

/// Hard cap on inline tool results, in bytes (`text.len()`) — results over
/// it spill to a file (proven default: Claude Code's ~30K).
pub const INLINE_CAP: usize = 30_000;

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
    /// P6-3 credential broker: bash injects declared secrets from it;
    /// `call()` sanitizes every ToolResult through it. None = no creds.
    pub broker: Option<crate::cred::Broker>,
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

/// One `run_code` sub-call, drained by the agent into an audit-only
/// `ScriptCall` event.
#[derive(Debug, Clone, PartialEq)]
pub struct ScriptRecord {
    pub name: String,
    /// First 12 hex chars of sha256 over the input JSON.
    pub input_digest: String,
    pub is_error: bool,
    pub denied: bool,
    pub raw_bytes: u64,
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
/// Sorted; `all_core_specs_deny_additional_properties` pins the count to
/// the registry's resident set. `mcp` is the one name that is not always
/// present: the spec exists only when a server is configured (`with_mcp`),
/// and it is never advertised — MCP tools are reached through `tools`.
pub const TOOL_NAMES: [&str; 18] = [
    "bash",
    "computer",
    "diagnostics",
    "edit",
    "glob",
    "grep",
    "mcp",
    "memory",
    "plan",
    "read",
    "repo_map",
    "run_code",
    "skill",
    "struct_search",
    "symbol",
    "task",
    "tools",
    "write",
];

pub use tools_tool::effective_call;

/// Which optional tools this host can serve, decided once per registry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Optional {
    /// At least one computer-use tier has a configured helper.
    pub computer: bool,
    /// `ast-grep`/`sg` or `semgrep` is configured or on PATH.
    pub struct_search: bool,
    /// A supported checker (`cargo`) is on PATH.
    pub diagnostics: bool,
    /// At least one skill exists ([`crate::skills::present`]).
    pub skill: bool,
}

impl Optional {
    /// Every optional tool on (fixed-environment tests).
    pub const ALL: Optional = Optional {
        computer: true,
        struct_search: true,
        diagnostics: true,
        skill: true,
    };

    /// Probe the host: env vars, PATH lookups and `is_file` checks only.
    /// Skills are looked up under `root` (the workspace) and `$HOME`.
    pub fn detect(root: &Path) -> Self {
        let backends = computer::Backends::detect();
        let bins = struct_search::Bins::detect();
        Optional {
            computer: backends.any_configured(),
            struct_search: bins.ast_grep.is_some() || bins.semgrep.is_some(),
            diagnostics: struct_search::find_on_path(&["cargo"]).is_some(),
            skill: crate::skills::present(root),
        }
    }

    /// `(tool, why it is absent and how to enable it)` for each tool off.
    fn absent(self) -> Vec<(&'static str, &'static str)> {
        let mut out = Vec::new();
        if !self.computer {
            out.push((
                "computer",
                "no computer-use backend is configured (put `cua-driver` on PATH or set \
                 OVERSEER_COMPUTER_DRIVER; legacy helpers: OVERSEER_COMPUTER_STRUCTURED, \
                 OVERSEER_COMPUTER_A11Y or OVERSEER_COMPUTER_PIXEL)",
            ));
        }
        if !self.diagnostics {
            out.push(("diagnostics", "no supported checker (`cargo`) is on PATH"));
        }
        if !self.skill {
            out.push((
                "skill",
                "no skills are installed (add <workspace>/.overseer/skills/<name>/SKILL.md \
                 or ~/.overseer/skills/<name>/SKILL.md)",
            ));
        }
        if !self.struct_search {
            out.push((
                "struct_search",
                "neither ast-grep/sg nor semgrep is on PATH (or set OVERSEER_AST_GREP / \
                 OVERSEER_SEMGREP)",
            ));
        }
        out
    }
}

pub struct ToolRegistry {
    /// Every spec resident in this registry — the set a mode or an
    /// ablation filters down from. `specs` is the advertised view.
    base_specs: Vec<crate::provider::ToolSpec>,
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
    /// Resident names kept out of the advertised array and reached through
    /// the `tools` dispatcher ([`tools_tool::DEFERRED`] ∩ `base_specs`).
    /// Subtracted by `rebuild_specs` alongside `disabled`; everything else
    /// (modes, ablation, `check_args`, search) still reads `base_specs`.
    deferred: HashSet<String>,
    /// P8-B hooks (ECC pattern): data rules loaded from
    /// `<root>/.overseer/hooks.json`. Pre rules block at the dispatch
    /// boundary (before the gate); post rules annotate the result. Empty
    /// when no file exists — hooks are an extra guardrail, never the gate.
    pub hooks: Vec<crate::hooks::HookRule>,
    /// P8-B session mode (roo pattern): the active posture's toolset and
    /// edit globs. `None` = default posture (every resident tool).
    pub mode: Option<&'static crate::modes::Mode>,
    /// Configured MCP servers + the live ones (`with_mcp`). `None` = no MCP
    /// in this registry, which is the shipped default. Discovered tool
    /// definitions never enter `base_specs`/`specs` — they live in here,
    /// found and called through `tools` (Invariant 2: the advertised array
    /// must not change because a third-party server did).
    mcp: Option<mcp_tool::McpState>,
    /// Optional tools this host cannot serve (see [`Optional`]): absent
    /// from the specs and refused at dispatch with the reason.
    unavailable: Vec<(&'static str, &'static str)>,
    /// Computer-tool state (S5): backends detected once at registry build
    /// — the spec never depends on which is present (D1) — plus the
    /// lazily-spawned cua-driver client when one is configured (D2).
    computer: computer::ComputerState,
    /// Memory v2 state behind the `memory` tool (and the engine's recall
    /// and reminder hooks).
    pub memory: memory_tool::MemoryState,
    /// Set while `run_code` re-enters `call` for a script's sub-call: read
    /// dedup is off and results cap at 1 MB inline instead of spilling.
    script: bool,
    /// `run_code` sub-calls since the last `take_script_calls`.
    script_calls: Vec<ScriptRecord>,
    /// The run's steering handle (user interrupt) for `run_code`.
    control: crate::control::Control,
}

impl ToolRegistry {
    /// The full resident registry for a session in `cwd`; optional tools
    /// per [`Optional::detect`], skills looked up under `cwd` — the root
    /// the prompt's skills segment indexes, not `policy.root` (`/` under
    /// full access).
    pub fn core_in(policy: crate::perm::Policy, cwd: &Path) -> Self {
        Self::core_with(policy, Optional::detect(cwd))
    }

    /// [`core_in`](Self::core_in) for tests, with `policy.root` as the cwd.
    #[cfg(test)]
    pub(crate) fn core(policy: crate::perm::Policy) -> Self {
        let cwd = policy.root.clone();
        Self::core_in(policy, &cwd)
    }

    /// [`core`](Self::core) with the optional-tool availability given.
    pub fn core_with(policy: crate::perm::Policy, optional: Optional) -> Self {
        // Sorted by name — the tool list serializes deterministically
        // regardless of registration order (Invariant 2: stable prefix).
        // `tools` is a placeholder here; `rebuild_specs` renders its
        // description from what this registry actually holds.
        #[allow(unused_mut)]
        let mut unavailable = optional.absent();
        #[cfg(not(feature = "code-mode"))]
        unavailable.push(("run_code", "this build has no code-mode feature"));
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
            diagnostics::spec(),
            struct_search::spec(),
            memory_tool::spec(),
            tools_tool::spec(&[], false),
        ];
        #[cfg(feature = "code-mode")]
        specs.push(run_code::spec());
        specs.retain(|s| !unavailable.iter().any(|(n, _)| *n == s.name));
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        let hooks = crate::hooks::load(&policy.root);
        let mut reg = ToolRegistry {
            base_specs: specs.clone(),
            specs,
            read_paths: HashSet::new(),
            read_log: HashMap::new(),
            policy,
            taint_notices: Vec::new(),
            disabled: HashSet::new(),
            deferred: tools_tool::DEFERRED.iter().map(|n| n.to_string()).collect(),
            hooks,
            mode: None,
            mcp: None,
            unavailable,
            computer: computer::ComputerState::detect(),
            memory: Default::default(),
            script: false,
            script_calls: Vec::new(),
            control: Default::default(),
        };
        reg.rebuild_specs();
        reg
    }

    /// Read-only registry for quarantined subagents (playbook Ch.3 §9.6):
    /// writes stay single-threaded in the parent agent. No `task` either —
    /// subagents cannot spawn subagents.
    pub fn readonly(policy: crate::perm::Policy) -> Self {
        let hooks = crate::hooks::load(&policy.root);
        let specs = vec![
            read::spec(),
            grep::spec(),
            glob::spec(),
            memory_tool::spec(),
        ];
        ToolRegistry {
            base_specs: specs.clone(),
            specs,
            read_paths: HashSet::new(),
            read_log: HashMap::new(),
            policy,
            taint_notices: Vec::new(),
            disabled: HashSet::new(),
            deferred: HashSet::new(),
            hooks,
            mode: None,
            mcp: None,
            unavailable: Vec::new(),
            computer: computer::ComputerState::detect(),
            memory: Default::default(),
            script: false,
            script_calls: Vec::new(),
            control: Default::default(),
        }
    }

    /// Plan-mode registry (P1.4 capability removal): mutating tools aren't
    /// merely denied — they're absent from the spec list, so the model
    /// cannot call them at all. Read tools + the plan artifact tool.
    pub fn plan_mode(policy: crate::perm::Policy) -> Self {
        let mut specs = vec![read::spec(), grep::spec(), glob::spec(), plan::spec()];
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        let hooks = crate::hooks::load(&policy.root);
        ToolRegistry {
            base_specs: specs.clone(),
            specs,
            read_paths: HashSet::new(),
            read_log: HashMap::new(),
            policy,
            taint_notices: Vec::new(),
            disabled: HashSet::new(),
            deferred: HashSet::new(),
            hooks,
            mode: None,
            mcp: None,
            unavailable: Vec::new(),
            computer: computer::ComputerState::detect(),
            memory: Default::default(),
            script: false,
            script_calls: Vec::new(),
            control: Default::default(),
        }
    }

    /// Install the configured MCP servers.
    ///
    /// Adds the internal `mcp` op spec to the resident set — deferred, so
    /// never advertised — and keeps every discovered tool definition in the
    /// MCP state, reached by name through `tools`. The only advertised
    /// change is `tools`' description gaining "MCP servers". That is the
    /// cache-stability contract (Invariant 2): a third-party server's names,
    /// count and descriptions must never enter the advertised array, or a
    /// server that reorders its list would invalidate the prompt prefix
    /// every turn.
    ///
    /// A no-op for an empty list, so a session with no configured server
    /// keeps a byte-identical spec array; the decision is taken here, once,
    /// and never revisited mid-session. The servers' declared `trust: read`
    /// ids are handed to the policy here too, which is the only place that
    /// sees the config.
    pub fn with_mcp(mut self, servers: Vec<crate::mcp_config::McpServer>) -> Self {
        if servers.is_empty() {
            return self;
        }
        self.policy.mcp_read_servers = mcp_tool::read_server_ids(&servers);
        self.base_specs.push(mcp_tool::spec());
        self.base_specs.sort_by(|a, b| a.name.cmp(&b.name));
        self.mcp = Some(mcp_tool::McpState::new(servers));
        self.rebuild_specs();
        self
    }

    /// Apply a session mode (roo pattern): every resident tool the mode
    /// does not allow is removed — spec list AND dispatch, the same
    /// capability-removal mechanism plan mode uses. The mode's complement
    /// replaces any previous mode's removals (postures don't stack), and
    /// `ablated` (`--no-tools`) is re-applied on top so an ablation is
    /// never resurrected by a mode switch.
    pub fn set_mode(&mut self, mode: &'static crate::modes::Mode, ablated: &[String]) {
        self.mode = Some(mode);
        self.disabled.clear();
        for n in mode.disable_list_for(&self.base_specs) {
            self.disabled.insert(n);
        }
        for n in ablated {
            self.disabled.insert(n.clone());
        }
        self.rebuild_specs();
    }

    /// Recompute the advertised list from the resident set minus the
    /// disabled and deferred sets (one source of truth for ablation + mode
    /// removal), and re-render `tools`' description from what is reachable
    /// behind it. Runs only at construction and mode/ablation changes, so
    /// the bytes are stable for the session.
    fn rebuild_specs(&mut self) {
        let catalog: Vec<&str> = self
            .base_specs
            .iter()
            .map(|s| s.name.as_str())
            .filter(|n| self.deferred.contains(*n) && !self.disabled.contains(*n))
            .collect();
        let tools = tools_tool::spec(&catalog, self.mcp_reachable());
        self.specs = self
            .base_specs
            .iter()
            .filter(|s| !self.disabled.contains(&s.name) && !self.deferred.contains(&s.name))
            .map(|s| {
                if s.name == "tools" {
                    tools.clone()
                } else {
                    s.clone()
                }
            })
            .collect();
    }

    /// Pin the computer backends (tests drive a fake cua-driver).
    #[cfg(test)]
    pub(crate) fn set_computer(&mut self, state: computer::ComputerState) {
        self.computer = state;
    }

    /// `name` is in this registry and neither disabled nor unavailable.
    pub fn reachable(&self, name: &str) -> bool {
        self.base_specs.iter().any(|s| s.name == name)
            && !self.disabled.contains(name)
            && !self.unavailable.iter().any(|(n, _)| *n == name)
    }

    /// Attach the run's steering handle: `run_code` aborts on its
    /// interrupt, between sub-calls and inside the JS interrupt handler.
    pub fn set_control(&mut self, control: crate::control::Control) {
        self.control = control;
    }

    /// Drain the `run_code` sub-call records of the last call.
    pub fn take_script_calls(&mut self) -> Vec<ScriptRecord> {
        std::mem::take(&mut self.script_calls)
    }

    /// MCP servers are configured and `mcp` is not ablated or mode-removed.
    fn mcp_reachable(&self) -> bool {
        self.mcp.is_some() && !self.disabled.contains("mcp")
    }

    /// The active mode's edit-glob verdict for `path` (no mode or empty
    /// globs → allowed). Called by the file tools so a mode's
    /// `edit_globs` bound cannot be bypassed by skipping the CLI.
    pub fn edit_allowed(&self, path: &str) -> bool {
        self.mode.is_none_or(|m| m.edit_allowed(path))
    }

    /// P4.3 ablation: drop `names` from the advertised spec list and refuse
    /// them at dispatch. Unknown names are ignored here — the CLI validates
    /// against TOOL_NAMES before this is ever called.
    pub fn disable(&mut self, names: &[String]) {
        for n in names {
            self.disabled.insert(n.clone());
        }
        self.rebuild_specs();
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
        // A script needs the content; its reads never entered context.
        if self.script {
            return false;
        }
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
        if !self.script {
            let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            self.read_log
                .entry(key)
                .or_default()
                .push(ReadRecord { mtime, start, end });
        }
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
        if let Some((_, why)) = self.unavailable.iter().find(|(n, _)| *n == name) {
            return ToolOutput::err(format!(
                "Tool '{name}' is not available in this session: {why}. Availability is \
                 detected once at startup — configure it, then start a new session."
            ));
        }
        // P8-B hooks (ECC pattern): a pre_tool_use rule blocks the call
        // BEFORE the gate — the first matching rule wins, and the verdict
        // is one-way (a hook can only tighten, never widen). Hooks are
        // data, not scripts: no subprocess, no second execution surface.
        if let Some(reason) = crate::hooks::maybe_block(&self.hooks, name, input) {
            return ToolOutput::denied(format!("hook `pre_tool_use` blocked it — {reason}"));
        }
        // RT-4: a bash command mentioning a brokered selector can exfil
        // the secret (echo $TOKEN > /tmp/x) — latch sensitive BEFORE the
        // gate so the triangle arms for this and follow-up side effects.
        // (The child still gets the real; the gate now Asks on the exfil.)
        if name == "bash" {
            if let (Some(cmd), Some(br)) = (
                input.get("command").and_then(|v| v.as_str()),
                ctx.broker.as_ref(),
            ) {
                if br.mentions_selector(cmd) {
                    if let Some(notice) = self.policy.mark_sensitive("broker") {
                        self.taint_notices.push(notice);
                    }
                }
            }
        }
        // B1-2 (Instructor/FastMCP): hand-rolled required/type check against
        // the advertised `input_schema` before the gate and dispatch — a
        // malformed call never reaches a human approval dialog. Zero-dep
        // (serde_json is already in the tree); failures return a field-level
        // error that names the violated field — no dispatch, no side effects.
        // Checked against `base_specs` so a deferred tool reached through
        // `tools` is validated against its own schema.
        if let Some(spec) = self.base_specs.iter().find(|s| s.name == name) {
            if let Err(e) = check_args(&spec.input_schema, input) {
                return e;
            }
        }
        match self.policy.gate(name, input) {
            crate::perm::Gate::Allow => {}
            crate::perm::Gate::Deny(reason) => {
                // F2: memory-gate quarantine — a denied memory write still
                // preserves its payload under memory/proposals/ for human
                // review (Ask headless-denies; the content must not drop).
                // Only fires when the reason names a quarantine redirect.
                if (name == "write" || name == "edit") && reason.contains("quarantined to ") {
                    if let Some(dest) = self.policy.proposal_path() {
                        let payload = input
                            .get("content")
                            .and_then(|v| v.as_str())
                            .or_else(|| input.get("new_string").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        if !payload.is_empty() {
                            if let Some(parent) = dest.parent() {
                                let _ = std::fs::create_dir_all(parent);
                            }
                            let _ = std::fs::write(&dest, payload);
                        }
                    }
                }
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
            "skill" => skill::run(input, ctx),
            "repo_map" => repomap::run_map(input, ctx),
            "symbol" => repomap::run_symbol(input, ctx),
            "computer" => computer::run(input, ctx, &mut self.computer),
            "diagnostics" => diagnostics::run(input, ctx),
            "struct_search" => struct_search::run(input, ctx),
            "memory" => memory_tool::run(input, ctx, &mut self.memory, &self.policy),
            // Re-enters `call` with the inner name: every step above and
            // below runs keyed on the tool actually executed.
            "tools" => tools_tool::run(input, ctx, self),
            #[cfg(feature = "code-mode")]
            "run_code" => run_code::run(input, ctx, self),
            // MCP (R1/R4): one internal op tool over the configured servers,
            // never advertised — `tools` re-enters here for `mcp__…` names,
            // and a resumed session's old `mcp` calls still land here. The
            // discovered tool definitions live in the state, never in
            // `specs`; a server is spawned on first use and dropped if it
            // dies. The result flows through the same post-hook, taint,
            // sanitize and budget path as every other tool below.
            "mcp" => match self.mcp.as_mut() {
                Some(state) => state.run(input),
                None => ToolOutput::err(
                    "mcp: no MCP servers are configured — add them to ~/.overseer/mcp.json.",
                ),
            },
            other => ToolOutput::err(format!(
                "Unknown tool '{other}'. Available tools: {}.",
                TOOL_NAMES.join(", ")
            )),
        };
        self.memory.observe(
            name,
            input,
            !out.is_error,
            &ctx.cwd,
            crate::memory::now_secs(),
        );
        // P8-B post_tool_use hooks: annotate (never block) the result so
        // the model sees the flagged property inline. Runs on the RAW text
        // — same ordering rule as the taint latch, which reads below.
        let out = match crate::hooks::post_notice(&self.hooks, name, &out.text) {
            Some(reason) => ToolOutput {
                text: format!("{}\n[hook] {reason}", out.text),
                ..out
            },
            None => out,
        };
        // Rule-of-Two bookkeeping (P3.10): this result may carry untrusted
        // content or secret material — latch on the RAW text before any
        // redaction (latch-order fix: sanitize must not blind the gate).
        if let Some(notice) = self.policy.note_result(name, input, &out.text) {
            self.taint_notices.push(notice);
        }
        // P6-3 broker sanitize: verbatim real→sentinel over the result
        // text AFTER note_result and BEFORE enforce_budget. This single
        // string flows into BOTH the model context and the ToolResult
        // event append, so events.jsonl is covered by construction.
        let out = match &ctx.broker {
            Some(br) if !br.is_empty() => ToolOutput {
                text: crate::cred::sanitize(br, &out.text),
                ..out
            },
            _ => out,
        };
        #[cfg(feature = "code-mode")]
        if self.script {
            return run_code::script_budget(out);
        }
        enforce_budget(out, ctx)
    }
}

/// Enforce the tool-result byte budget (playbook Ch.6 §2.2):
/// ≤[`INLINE_CAP`] bytes inline; larger results spill to a file and return a
/// pointer.
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
    // P6-3 scan: curated secret families redact BEFORE the spill write —
    // the spilled file, the preview, and the metadata log never hold
    // plaintext. Span-only redact + notice (no secret content copied).
    let (scan_text, scan_notice) = crate::cred::redact(&out.text);
    let out = ToolOutput {
        text: scan_text,
        ..out
    };
    if !scan_notice.is_empty() {
        crate::cred::note_redaction(&scan_notice);
    }
    let dir = ctx.session_dir.join("tool-outputs");
    let _ = std::fs::create_dir_all(&dir);
    #[cfg(unix)]
    {
        // Spill dirs 0700 (R1-F4/F7): tool output may carry secrets.
        // DirBuilder.mode is a no-op when the dir already exists, so set
        // permissions explicitly after creation.
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    ctx.spill_seq += 1;
    let path = dir.join(format!("output-{}.txt", ctx.spill_seq));
    let raw = out.text.clone();
    let preview: String = raw.chars().take(4_000).collect();
    let size = raw.len();
    match spill_write(&path, &raw) {
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

/// Spill write with 0600 file perms on unix (R1-F4): tool output may
/// carry secrets; non-unix is best-effort (no mode API).
fn spill_write(path: &std::path::Path, raw: &str) -> std::io::Result<()> {
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
                f.write_all(raw.as_bytes())
            })
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, raw)
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

/// `O_NOFOLLOW` per target (no libc crate): the open fails with `ELOOP`
/// when the final path component is a symlink.
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "openbsd",
    target_os = "netbsd",
    target_os = "dragonfly"
))]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64"
    )
))]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    not(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc",
        target_arch = "powerpc64"
    ))
))]
const O_NOFOLLOW: i32 = 0o400000;
#[cfg(all(
    unix,
    not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))
))]
const O_NOFOLLOW: i32 = 0;

/// Canonicalize the longest existing ancestor of `p` and re-append the
/// missing remainder (the same shape as the permission gate's check).
fn canon_deep(p: &Path) -> PathBuf {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = cur.canonicalize() {
            let mut out = c;
            for comp in missing.iter().rev() {
                out.push(comp);
            }
            return out;
        }
        match cur.file_name() {
            Some(name) => {
                missing.push(name.to_os_string());
                cur.pop();
            }
            None => return p.to_path_buf(),
        }
    }
}

/// Resolve the file a write/edit will touch, immediately before the write:
/// create missing parents (only after the deepest existing ancestor is
/// proven contained), canonicalize the PARENT, check it sits under `root`,
/// and resolve a final-component symlink to its (contained) target. The
/// returned canonical path is what `snapshot` records and what
/// [`write_no_follow`] opens — a symlink swapped in after the gate's
/// containment check can no longer redirect the write.
// DEFERRED(owner): fd-relative (openat) path walk closes the
// intermediate-dir TOCTOU — including the `create_dir_all` side-effect
// window below: under a symlink race the mkdirs can still create
// directories OUTSIDE `root` before the post-canonicalize `starts_with`
// check refuses the write (the write itself is blocked; the stray dirs
// are not).
pub(crate) fn contained_target(path: &Path, root: &Path) -> Result<PathBuf, String> {
    let root = canon_deep(root);
    let outside = |p: &Path| {
        format!(
            "Refusing to write {}: it resolves to {}, outside the working directory {}.",
            path.display(),
            p.display(),
            root.display()
        )
    };
    let (Some(parent), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(format!("Cannot write {}: not a file path.", path.display()));
    };
    let pre = canon_deep(parent);
    if !pre.starts_with(&root) {
        return Err(outside(&pre));
    }
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("Cannot create {}: {e}", parent.display()))?;
    let parent = parent
        .canonicalize()
        .map_err(|e| format!("Cannot resolve {}: {e}", parent.display()))?;
    if !parent.starts_with(&root) {
        return Err(outside(&parent));
    }
    let target = parent.join(name);
    let is_link = std::fs::symlink_metadata(&target).is_ok_and(|m| m.file_type().is_symlink());
    if !is_link {
        return Ok(target);
    }
    let resolved = target.canonicalize().map_err(|e| {
        format!(
            "Refusing to write {}: dangling symlink ({e}).",
            path.display()
        )
    })?;
    if !resolved.starts_with(&root) {
        return Err(outside(&resolved));
    }
    Ok(resolved)
}

/// Create/truncate `target` and write `content` through a handle opened
/// with `O_NOFOLLOW`: if the final component is (or became) a symlink,
/// the open fails instead of writing through it.
pub(crate) fn write_no_follow(target: &Path, content: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.custom_flags(O_NOFOLLOW);
        if O_NOFOLLOW == 0
            && std::fs::symlink_metadata(target).is_ok_and(|m| m.file_type().is_symlink())
        {
            return Err(format!(
                "Cannot write {}: it is a symlink.",
                target.display()
            ));
        }
    }
    let mut f = opts
        .open(target)
        .map_err(|e| format!("Cannot write {}: {e}", target.display()))?;
    f.write_all(content)
        .map_err(|e| format!("Cannot write {}: {e}", target.display()))
}

/// Keep the env pairs whose key passes `keep`. Built on `vars_os` so a
/// non-UTF-8 entry never panics: a non-UTF-8 key cannot match an allowlist
/// and is skipped; values pass through byte-exact.
pub(crate) fn filter_env(
    vars: impl IntoIterator<Item = (std::ffi::OsString, std::ffi::OsString)>,
    keep: impl Fn(&str) -> bool,
) -> Vec<(std::ffi::OsString, std::ffi::OsString)> {
    vars.into_iter()
        .filter(|(k, _)| k.to_str().is_some_and(&keep))
        .collect()
}

/// `cmd.spawn()`, retried briefly on `ETXTBSY`: an executable that was just
/// written can stay "busy" while a concurrently forked child still holds
/// the writer's fd until its own exec.
pub(crate) fn spawn_retrying_busy(
    cmd: &mut std::process::Command,
) -> std::io::Result<std::process::Child> {
    const ETXTBSY: i32 = 26;
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && tries < 20 => {
                tries += 1;
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            other => return other,
        }
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
            broker: None,
        }
    }

    #[test]
    fn memory_gate_quarantine_preserves_payload() {
        // F2: headless Ask→Deny on a memory write still lands the payload
        // under memory/proposals/ for human review (nothing dropped).
        use serde_json::json;
        let dir = tmpdir();
        let mem = dir.join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        let mut pol = crate::perm::Policy::headless(dir.clone());
        pol.memory_dir = Some(mem.clone());
        pol.note_result("task", &json!({}), "some task digest");
        pol.note_result("read", &json!({"path": ".env"}), "export KEY=1");
        assert!(pol.taint_armed());
        let mut reg = ToolRegistry::core(pol);
        let mut c = ctx(&dir);
        let out = reg.call(
            "write",
            &json!({"path": "memory/episodic/diary.md", "content": "UNTRUSTED-NOTE-42"}),
            &mut c,
        );
        assert!(
            out.denied,
            "memory write must deny headless, got: {}",
            out.text
        );
        let props = mem.join("proposals");
        let found: Vec<_> = std::fs::read_dir(&props)
            .expect("proposals dir must exist")
            .flatten()
            .collect();
        assert_eq!(found.len(), 1, "exactly one quarantined payload");
        let body = std::fs::read_to_string(found[0].path()).unwrap();
        assert!(
            body.contains("UNTRUSTED-NOTE-42"),
            "payload preserved, got: {body}"
        );
    }

    #[test]
    fn bash_mentioning_brokered_selector_latches_sensitive() {
        // RT-4: `echo $TOKEN > /tmp/x` must arm the triangle's sensitive half.
        let dir = tmpdir();
        let mut br = crate::cred::Broker::new();
        br.issue_capability("a", "API_TOKEN", "real-secret-1", vec![], vec![], None);
        let pol = crate::perm::Policy::headless(dir.clone());
        let mut reg = ToolRegistry::core(pol);
        let mut c = ctx(&dir);
        c.broker = Some(br);
        let _ = reg.call(
            "bash",
            &serde_json::json!({"command": "echo $API_TOKEN > /tmp/rt4-x"}),
            &mut c,
        );
        assert!(
            reg.policy().taint_sensitive(),
            "brokered selector in bash must latch sensitive"
        );
    }

    #[test]
    fn memory_gate_burst_preserves_every_payload() {
        // F2 (extreme): 100 same-ms gated writes → 100 files, zero loss.
        let dir = tmpdir();
        let mem = dir.join("memory");
        std::fs::create_dir_all(&mem).unwrap();
        let mut pol = crate::perm::Policy::headless(dir.clone());
        pol.memory_dir = Some(mem.clone());
        pol.note_result("task", &serde_json::json!({}), "some task digest");
        pol.note_result("read", &serde_json::json!({"path": ".env"}), "export KEY=1");
        assert!(pol.taint_armed());
        let mut reg = ToolRegistry::core(pol);
        let mut c = ctx(&dir);
        for i in 0..100 {
            let out = reg.call(
                "write",
                &serde_json::json!({"path": format!("memory/note-{i}.md"), "content": format!("PAYLOAD-{i}")}),
                &mut c,
            );
            assert!(out.denied, "write {i} must deny headless");
        }
        let props = mem.join("proposals");
        let files: Vec<_> = std::fs::read_dir(&props).unwrap().flatten().collect();
        assert_eq!(files.len(), 100, "every payload gets its own file");
        let bodies: Vec<String> = files
            .iter()
            .map(|f| std::fs::read_to_string(f.path()).unwrap())
            .collect();
        for i in 0..100 {
            assert!(
                bodies.iter().any(|b| b.contains(&format!("PAYLOAD-{i}"))),
                "payload {i} kept"
            );
        }
    }

    #[test]
    fn a_malformed_call_never_reaches_the_permission_gate() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let dir = tmpdir();
        let asks = Arc::new(AtomicUsize::new(0));
        let seen = asks.clone();
        let mut pol = crate::perm::Policy::preset(crate::perm::Preset::WorkspaceWrite, dir.clone());
        pol.ask_handler = Some(crate::perm::AskHandler(Arc::new(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            crate::perm::AskDecision::Deny
        })));
        let mut reg = ToolRegistry::core(pol);
        let mut c = ctx(&dir);
        // `git push` Asks — but a wrong-typed field must fail first.
        for bad in [
            serde_json::json!({"command": "git push", "timeout_ms": "soon"}),
            serde_json::json!({"command": "git push", "bogus": 1}),
        ] {
            let out = reg.call("bash", &bad, &mut c);
            assert!(out.is_error && !out.denied, "{}", out.text);
        }
        assert_eq!(
            asks.load(Ordering::SeqCst),
            0,
            "no dialog for a malformed call"
        );
        let out = reg.call("bash", &serde_json::json!({"command": "git push"}), &mut c);
        assert!(out.denied, "{}", out.text);
        assert_eq!(
            asks.load(Ordering::SeqCst),
            1,
            "a well-formed call is gated"
        );
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
    fn unconfigured_optional_tools_are_absent_and_refused() {
        let dir = tmpdir();
        let mut reg =
            ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::default());
        let names: Vec<&str> = reg.specs.iter().map(|s| s.name.as_str()).collect();
        for t in ["computer", "struct_search", "diagnostics"] {
            assert!(!names.contains(&t), "{t} must not be advertised");
        }
        assert!(names.contains(&"read") && names.contains(&"bash"));
        let mut c = ctx(&dir);
        let out = reg.call("struct_search", &json!({"pattern": "foo($A)"}), &mut c);
        assert!(out.is_error);
        assert!(
            out.text.contains("not available in this session"),
            "{}",
            out.text
        );
        assert!(out.text.contains("semgrep"), "{}", out.text);
        let full = ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::ALL);
        for t in ["computer", "struct_search", "diagnostics"] {
            assert!(full.base_specs.iter().any(|s| s.name == t), "{t} forced on");
        }
    }

    #[test]
    fn spec_array_is_byte_stable_and_name_sorted_across_builds() {
        let ser = |r: &ToolRegistry| serde_json::to_string(&spec_json(&r.specs)).unwrap();
        let a = ToolRegistry::core(crate::perm::Policy::allow_all());
        let b = ToolRegistry::core(crate::perm::Policy::allow_all());
        assert_eq!(ser(&a), ser(&b), "same environment, same bytes");
        for reg in [
            &a,
            &ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::ALL),
        ] {
            let names: Vec<&str> = reg.specs.iter().map(|s| s.name.as_str()).collect();
            let mut sorted = names.clone();
            sorted.sort_unstable();
            assert_eq!(names, sorted);
        }
    }

    fn spec_json(specs: &[crate::provider::ToolSpec]) -> Vec<Value> {
        specs
            .iter()
            .map(|s| json!({"name": s.name, "description": s.description, "input_schema": s.input_schema}))
            .collect()
    }

    /// Serialized `name + description + input_schema` chars of one spec —
    /// the three fields every provider sends.
    fn spec_chars(s: &crate::provider::ToolSpec) -> usize {
        s.name.chars().count()
            + s.description.chars().count()
            + serde_json::to_string(&s.input_schema)
                .unwrap()
                .chars()
                .count()
    }

    /// Startup-token guard: the advertised array with every optional tool
    /// available, MCP configured and skills present. The deferred tools
    /// ride behind `tools`, so only the resident set counts: 9,886 chars
    /// before deferral; tool economy measured 5,375 and memory v2's
    /// `memory` spec adds 552 (5,927). ≤ 6,000 is the budget, and the exact total
    /// is pinned with +5% headroom.
    #[test]
    fn resident_tool_specs_stay_within_the_startup_budget() {
        const BUDGET_CHARS: usize = 6_000;
        const RESIDENT_CHARS: usize = 5_927;
        let reg = ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::ALL)
            .with_mcp(vec![crate::mcp_config::McpServer {
                name: "s".into(),
                command: "/bin/false".into(),
                args: Vec::new(),
                env: Default::default(),
                trust: crate::mcp_config::Trust::Ask,
            }]);
        for s in &reg.specs {
            println!("spec {:<14} {:>5} chars", s.name, spec_chars(s));
        }
        let total: usize = reg.specs.iter().map(spec_chars).sum();
        println!("spec TOTAL {total} chars (~{} tokens)", total / 4);
        assert!(total <= BUDGET_CHARS, "resident specs are {total} chars");
        assert!(
            total <= RESIDENT_CHARS + RESIDENT_CHARS / 20,
            "resident tool specs grew to {total} chars (pinned {RESIDENT_CHARS} + 5%)"
        );
    }

    #[cfg(not(feature = "code-mode"))]
    #[test]
    fn run_code_is_refused_without_code_mode() {
        let dir = tmpdir();
        let mut reg = ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::ALL);
        assert!(reg.specs.iter().all(|s| s.name != "run_code"));
        let out = reg.call("run_code", &json!({"code": "return 1;"}), &mut ctx(&dir));
        assert!(out.is_error, "{}", out.text);
        assert!(
            out.text
                .contains("not available in this session: this build has no code-mode feature"),
            "{}",
            out.text
        );
    }

    #[test]
    fn all_core_specs_deny_additional_properties() {
        // B1-2 FastMCP audit: every resident spec (advertised or deferred)
        // must be strict.
        let reg = ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::ALL);
        for spec in &reg.base_specs {
            assert_eq!(
                spec.input_schema.get("additionalProperties"),
                Some(&serde_json::Value::Bool(false)),
                "spec {} must set additionalProperties:false",
                spec.name
            );
        }
        // `mcp` is only resident when a server is configured and
        // `run_code` only in a code-mode build, so the count compares
        // against TOOL_NAMES minus those — the intent (spec list ↔
        // validation set, one of each) is kept.
        let resident: Vec<&str> = TOOL_NAMES
            .iter()
            .copied()
            .filter(|name| *name != "mcp")
            .filter(|name| cfg!(feature = "code-mode") || *name != "run_code")
            .collect();
        assert_eq!(reg.base_specs.len(), resident.len());
        assert!(!reg.base_specs.iter().any(|s| s.name == "mcp"));
        assert!(!reg.specs.iter().any(|s| s.name == "mcp"));
    }

    #[test]
    fn hook_rules_block_and_annotate_at_dispatch() {
        // P8-B accept (ECC hooks): a pre rule denies before the gate (the
        // call never runs), a post rule annotates the result text.
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join(".overseer")).unwrap();
        std::fs::write(
            dir.join(crate::hooks::HOOKS_FILE),
            r#"[
              {"event":"pre_tool_use","tool":"bash","contains":"curl","reason":"no egress from this repo"},
              {"event":"post_tool_use","tool":"read","contains":"SECRET_MARKER","reason":"file carries a marked value"}
            ]"#,
        )
        .unwrap();
        let mut reg = ToolRegistry::core(crate::perm::Policy::headless(dir.clone()));
        assert_eq!(reg.hooks.len(), 2, "the rules file is loaded");
        let mut c = ctx(&dir);

        let blocked = reg.call(
            "bash",
            &json!({"command": "curl https://example.com"}),
            &mut c,
        );
        assert!(blocked.denied, "pre-hook must deny the call");
        assert!(
            blocked.text.contains("no egress from this repo"),
            "{}",
            blocked.text
        );
        assert!(
            blocked.text.contains("Permission denied"),
            "a hook block is a gate-shaped deny: {}",
            blocked.text
        );

        std::fs::write(dir.join("n.txt"), "SECRET_MARKER here\n").unwrap();
        let annotated = reg.call("read", &json!({"path": "n.txt"}), &mut c);
        assert!(!annotated.is_error);
        assert!(
            annotated
                .text
                .contains("[hook] file carries a marked value"),
            "{}",
            annotated.text
        );
        // A clean result is untouched.
        std::fs::write(dir.join("m.txt"), "nothing to see\n").unwrap();
        let clean = reg.call("read", &json!({"path": "m.txt"}), &mut c);
        assert!(!clean.text.contains("[hook]"), "{}", clean.text);
    }

    #[test]
    fn mode_removes_tools_and_a_later_mode_restores_them() {
        // P8-B accept (roo modes): capability removal, reversible by a
        // switch back — the resident set (`base_specs`) is the source.
        let dir = tmpdir();
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        reg.set_mode(crate::modes::for_mode("architect").unwrap(), &[]);
        let names: Vec<&str> = reg.specs.iter().map(|s| s.name.as_str()).collect();
        assert!(!names.contains(&"write") && !names.contains(&"bash"));
        assert!(names.contains(&"read"));
        let mut c = ctx(&dir);
        let out = reg.call("write", &json!({"path": "a.txt", "content": "x"}), &mut c);
        assert!(out.is_error, "a removed tool is refused at dispatch");
        assert!(out.text.contains("disabled for this run"), "{}", out.text);

        // Switching back to the default posture restores the full set —
        // but re-applies an ablation passed alongside.
        reg.set_mode(
            crate::modes::for_mode("code").unwrap(),
            &["computer".to_string()],
        );
        let names: Vec<&str> = reg.specs.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"write"));
        assert!(
            !names.contains(&"computer"),
            "--no-tools survives a mode switch"
        );
    }

    #[test]
    fn diagnostics_is_resident_and_arg_checked() {
        let dir = tmpdir();
        let mut reg = ToolRegistry::core_with(crate::perm::Policy::allow_all(), Optional::ALL);
        assert!(
            reg.base_specs.iter().any(|s| s.name == "diagnostics")
                && !reg.specs.iter().any(|s| s.name == "diagnostics"),
            "the diagnostics tool is resident but deferred"
        );
        let mut c = ctx(&dir);
        // A typo'd argument is refused before the tool runs (B1-2).
        let out = reg.call("diagnostics", &json!({"bogus": 1}), &mut c);
        assert!(out.is_error);
        assert!(
            out.text.contains("Unknown parameter 'bogus'"),
            "{}",
            out.text
        );
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

    #[cfg(unix)]
    #[test]
    fn filter_env_survives_non_utf8_entries() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let bad = OsString::from_vec(vec![b'x', 0xff]);
        let vars = vec![
            (OsString::from("LC_CTYPE"), bad.clone()),
            (
                OsString::from_vec(vec![b'L', b'C', b'_', 0xfe]),
                OsString::from("v"),
            ),
            (OsString::from("FOO_API_KEY"), OsString::from("secret")),
            (OsString::from("PATH"), OsString::from("/bin")),
        ];
        let kept = filter_env(vars, |k| k == "PATH" || k.starts_with("LC_"));
        assert_eq!(
            kept,
            vec![
                (OsString::from("LC_CTYPE"), bad),
                (OsString::from("PATH"), OsString::from("/bin")),
            ]
        );
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
            broker: None,
        };

        // Overwriting an existing file needs a read first (T3).
        reg.call("read", &json!({"path": "old.txt"}), &mut c);
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

    #[test]
    fn broker_sanitize_runs_after_latch_before_budget() {
        // P6-3 accept: a tool result carrying a mapped real comes back
        // with the sentinel, not the real — and the taint latch still
        // saw the raw text (latch-order: note_result runs first).
        // Uses the read tool (no child spawn — hermetic under load).
        let dir = tmpdir();
        std::fs::write(dir.join("secret.txt"), "password is pw-real-9 ok\n").unwrap();
        let mut br = crate::cred::Broker::new();
        let sentinel = br.issue_capability(
            "db",
            "DB_PASS",
            "pw-real-9",
            vec![],
            vec!["read".into()],
            None,
        );
        let mut reg = ToolRegistry::core(crate::perm::Policy::allow_all());
        let mut c = ctx(&dir);
        c.broker = Some(br);
        let out = reg.call("read", &serde_json::json!({"path": "secret.txt"}), &mut c);
        assert!(!out.is_error, "got: {}", out.text);
        assert!(!out.text.contains("pw-real-9"), "got: {}", out.text);
        assert!(out.text.contains(&sentinel), "got: {}", out.text);
    }

    #[test]
    fn spill_files_are_owner_only() {
        // P6-3 accept (spill-perms): over-cap output spills to a 0600
        // file under a 0700 dir (unix; best-effort elsewhere).
        let dir = tmpdir();
        std::fs::create_dir_all(dir.join("session")).unwrap();
        let mut c = ctx(&dir);
        c.session_dir = dir.join("session");
        let big = "y".repeat(crate::cred::scan("plain").len() + INLINE_CAP + 100);
        let out = enforce_budget(ToolOutput::ok(big), &mut c);
        let spilled = out.spilled_to.expect("over-cap must spill");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&spilled).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "spill file mode {mode:o}");
            let dmode = std::fs::metadata(dir.join("session").join("tool-outputs"))
                .unwrap()
                .permissions()
                .mode();
            // F7: the dir itself must be owner-only (set_permissions
            // after creation tightens pre-existing dirs too).
            assert_eq!(dmode & 0o777, 0o700, "spill dir mode {dmode:o}");
        }
    }
}
